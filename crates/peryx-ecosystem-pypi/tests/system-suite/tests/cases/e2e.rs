#![cfg(feature = "e2e")]

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{Cursor, Read as _, Write as _};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::thread::JoinHandle;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use peryx_storage::blob::Digest;
use peryx_test_support::{Node, ProcessHarness, ProcessLimit, peryx_binary};
use rstest::rstest;
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

const SIMPLE_JSON_CT: &str = "application/vnd.pypi.simple.v1+json";
const SERVER_STARTUP_TIMEOUT: Duration = Duration::from_secs(20);

const UPLOAD_TOKEN: &str = "e2e-upload-secret";

struct Dist {
    name: String,
    version: String,
    wheel: Vec<u8>,
    metadata: Vec<u8>,
}

impl Dist {
    fn wheel_filename(&self) -> String {
        format!("{}-{}-py3-none-any.whl", self.name, self.version)
    }
}

fn build_dist(name: &str, version: &str, requires: &[&str]) -> Dist {
    let dist_info = format!("{name}-{version}.dist-info");
    let mut metadata = format!("Metadata-Version: 2.1\nName: {name}\nVersion: {version}\nRequires-Python: >=3.8\n");
    for dep in requires {
        writeln!(metadata, "Requires-Dist: {dep}").expect("write metadata");
    }
    let wheel_meta = "Wheel-Version: 1.0\nGenerator: peryx-e2e\nRoot-Is-Purelib: true\nTag: py3-none-any\n";
    let init = format!("VALUE = {name:?}\n");
    let init_path = format!("{name}/__init__.py");
    let metadata_path = format!("{dist_info}/METADATA");
    let wheel_path = format!("{dist_info}/WHEEL");
    let record_path = format!("{dist_info}/RECORD");
    let record_entries: [(&str, &[u8]); 3] = [
        (init_path.as_str(), init.as_bytes()),
        (metadata_path.as_str(), metadata.as_bytes()),
        (wheel_path.as_str(), wheel_meta.as_bytes()),
    ];
    let mut record = String::new();
    for (path, content) in record_entries {
        writeln!(
            record,
            "{path},sha256={},{}",
            URL_SAFE_NO_PAD.encode(Sha256::digest(content)),
            content.len()
        )
        .expect("write record");
    }
    writeln!(record, "{record_path},,").expect("write record");
    let mut buf = Vec::new();
    {
        let entries: [(&str, &[u8]); 4] = [
            (init_path.as_str(), init.as_bytes()),
            (metadata_path.as_str(), metadata.as_bytes()),
            (wheel_path.as_str(), wheel_meta.as_bytes()),
            (record_path.as_str(), record.as_bytes()),
        ];
        let mut zip = zip::ZipWriter::new(Cursor::new(&mut buf));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (path, content) in &entries {
            zip.start_file(*path, options).expect("zip entry");
            zip.write_all(content).expect("zip write");
        }
        zip.finish().expect("zip finish");
    }
    Dist {
        name: name.to_owned(),
        version: version.to_owned(),
        wheel: buf,
        metadata: metadata.into_bytes(),
    }
}

fn simple_json(dist: &Dist, port: u16) -> Vec<u8> {
    let wheel = dist.wheel_filename();
    let json = serde_json::json!({
        "meta": {"api-version": "1.1"},
        "name": dist.name,
        "versions": [dist.version],
        "files": [{
            "filename": wheel,
            "url": format!("http://127.0.0.1:{port}/files/{wheel}"),
            "hashes": {"sha256": Digest::of(&dist.wheel).as_str()},
            "requires-python": ">=3.8",
            "size": dist.wheel.len(),
            "upload-time": "2020-01-01T00:00:00Z",
            "core-metadata": {"sha256": Digest::of(&dist.metadata).as_str()},
        }],
    });
    serde_json::to_vec(&json).expect("serialize simple json")
}

type Routes = HashMap<String, (String, Vec<u8>)>;

struct Upstream {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Upstream {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
        let port = listener.local_addr().expect("addr").port();
        let dists = [
            build_dist("peryxa", "1.0", &["peryxb"]),
            build_dist("peryxb", "1.0", &[]),
            build_dist("peryxc", "1.0", &[]),
        ];
        let mut routes: Routes = HashMap::new();
        for dist in dists {
            let wheel = dist.wheel_filename();
            routes.insert(
                format!("/simple/{}/", dist.name),
                (SIMPLE_JSON_CT.to_owned(), simple_json(&dist, port)),
            );
            let octet = "application/octet-stream".to_owned();
            routes.insert(format!("/files/{wheel}"), (octet.clone(), dist.wheel));
            routes.insert(format!("/files/{wheel}.metadata"), (octet, dist.metadata));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let routes = Arc::new(routes);
        let handle = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || serve(&listener, &routes, &stop))
        };
        Self {
            port,
            stop,
            handle: Some(handle),
        }
    }

    fn upstream_url(&self) -> String {
        format!("http://127.0.0.1:{}/simple/", self.port)
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve(listener: &TcpListener, routes: &Arc<Routes>, stop: &Arc<AtomicBool>) {
    let mut responders = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let (socket, _) = listener.accept().expect("accept fixture request");
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let routes = Arc::clone(routes);
        responders.push(std::thread::spawn(move || respond(socket, &routes)));
    }
    let mut panicked = false;
    for responder in responders {
        panicked |= responder.join().is_err();
    }
    assert!(!panicked, "fixture responder panicked");
}

fn respond(mut socket: TcpStream, routes: &Routes) {
    // macOS inherits non-blocking mode from the listener.
    socket.set_nonblocking(false).expect("blocking socket");
    socket.set_read_timeout(Some(Duration::from_secs(5))).ok();
    let mut request = Vec::new();
    let mut chunk = [0_u8; 1024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        match socket.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => request.extend_from_slice(&chunk[..n]),
        }
    }
    let path = String::from_utf8_lossy(&request)
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .map(|target| target.split('?').next().unwrap_or(target).to_owned())
        .unwrap_or_default();

    let (status, ctype, body): (&str, &str, &[u8]) = match routes.get(&path) {
        Some((ctype, body)) => ("200 OK", ctype.as_str(), body.as_slice()),
        None => ("404 Not Found", "text/plain", b"not found".as_slice()),
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = socket.write_all(head.as_bytes());
    let _ = socket.write_all(body);
}

static SERVER_LIMIT: LazyLock<ProcessLimit> = LazyLock::new(|| ProcessLimit::new(4));

struct Peryx {
    node: Node,
    port: u16,
}

impl Peryx {
    fn start_against(upstream_url: &str) -> Self {
        Self::start_against_with_overlay_policy(upstream_url, "")
    }

    fn start_against_with_overlay_policy(upstream_url: &str, policy_toml: &str) -> Self {
        let config_toml = format!(
            "[[index]]\nname = \"upstream\"\nroute = \"upstream\"\n\
             [[index.upstream]]\nname = \"primary\"\nurl = \"{upstream_url}\"\n\
             [[index]]\nname = \"hosted\"\nhosted = true\n\
             [[index.access_token]]\nname = \"uploader\"\nsecret = \"{UPLOAD_TOKEN}\"\nactions = [\"write\", \"delete\"]\n\
             [[index]]\nname = \"root-pypi\"\nroute = \"root/pypi\"\nlayers = [\"hosted\", \"upstream\"]\nwrite_target = \"hosted\"\n\
             [index.policy]\n{policy_toml}"
        );
        let node = ProcessHarness::new(peryx_binary())
            .with_ready_timeout(SERVER_STARTUP_TIMEOUT)
            .with_process_limit(SERVER_LIMIT.clone())
            .spawn_with_config("pypi-e2e", &config_toml)
            .unwrap_or_else(|error| panic!("{error}"));
        Self {
            port: node.port(),
            node,
        }
    }

    fn server_log(&self) -> String {
        self.node.log()
    }

    fn index_url(&self) -> String {
        format!("http://127.0.0.1:{}/root/pypi/simple/", self.port)
    }

    fn upload_url(&self) -> String {
        format!("http://127.0.0.1:{}/root/pypi/", self.port)
    }

    fn metadata_requests(&self) -> u64 {
        let (status, body) = http_get(self.port, "/metrics").expect("metrics");
        assert_eq!(status, 200);
        sum_labeled_counter(&body, "peryx_metadata_served_total")
    }
}

fn hermetic() -> (Upstream, Peryx) {
    let upstream = Upstream::start();
    let peryx = Peryx::start_against(&upstream.upstream_url());
    (upstream, peryx)
}

fn hermetic_with_overlay_policy(policy_toml: &str) -> (Upstream, Peryx) {
    let upstream = Upstream::start();
    let peryx = Peryx::start_against_with_overlay_policy(&upstream.upstream_url(), policy_toml);
    (upstream, peryx)
}

fn http_get(port: u16, path: &str) -> Option<(u16, String)> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream
        .write_all(
            format!(
                "GET {path} HTTP/1.0\r\nHost: localhost\r\n\
                 Accept: application/vnd.pypi.simple.v1+json\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .expect("write request");
    let mut raw = String::new();
    stream.read_to_string(&mut raw).expect("read response");
    let (head, body) = raw.split_once("\r\n\r\n").expect("http response has a body");
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .expect("status code");
    Some((status, body.to_owned()))
}

fn http_get_bytes(port: u16, path: &str) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .write_all(format!("GET {path} HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n").as_bytes())
        .expect("write request");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read response");
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("http response has a body");
    let status = std::str::from_utf8(&raw[..split])
        .ok()
        .and_then(|head| head.lines().next())
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .expect("status code");
    (status, raw[split + 4..].to_vec())
}

fn html_get(port: u16, path: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .write_all(
            format!("GET {path} HTTP/1.0\r\nHost: localhost\r\nAccept: text/html\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .expect("write");
    let mut raw = String::new();
    stream.read_to_string(&mut raw).expect("read");
    raw.split_once("\r\n\r\n").expect("body").1.to_owned()
}

fn sum_labeled_counter(metrics: &str, name: &str) -> u64 {
    metrics
        .lines()
        .filter_map(|line| line.strip_prefix(name)?.rsplit_once('}')?.1.trim().parse::<u64>().ok())
        .sum()
}

fn uv_venv() -> TempDir {
    let dir = TempDir::new().expect("venv dir");
    // `uv venv` rejects a target pre-populated by `UV_CACHE_DIR`.
    run(Command::new("uv").arg("venv").arg(dir.path()), "uv venv");
    dir
}

fn venv_python(venv: &TempDir) -> PathBuf {
    venv.path().join("bin").join("python")
}

fn pip_install(venv: &TempDir, peryx: &Peryx, spec: &str) {
    let mut cmd = Command::new("pip3");
    cmd.arg("--python").arg(venv_python(venv)).args([
        "install",
        "--no-cache-dir",
        "--no-input",
        "--index-url",
        &peryx.index_url(),
        spec,
    ]);
    run_against(&mut cmd, "pip install", peryx);
}

fn pip_install_fails(venv: &TempDir, peryx: &Peryx, spec: &str) {
    let mut cmd = Command::new("pip3");
    cmd.arg("--python").arg(venv_python(venv)).args([
        "install",
        "--no-cache-dir",
        "--no-input",
        "--index-url",
        &peryx.index_url(),
        spec,
    ]);
    run_against_fails(&mut cmd, "pip install", peryx);
}

fn uv_install(venv: &TempDir, peryx: &Peryx, spec: &str) {
    let mut cmd = uv(venv);
    cmd.args(["pip", "install", "--python"])
        .arg(venv_python(venv))
        .args(["--index-url", &peryx.index_url(), spec]);
    run_against(&mut cmd, "uv pip install", peryx);
}

fn uv_install_fails(venv: &TempDir, peryx: &Peryx, spec: &str) {
    let mut cmd = uv(venv);
    cmd.args(["pip", "install", "--python"])
        .arg(venv_python(venv))
        .args(["--index-url", &peryx.index_url(), spec]);
    run_against_fails(&mut cmd, "uv pip install", peryx);
}

#[derive(Clone, Copy)]
enum Client {
    Pip,
    Uv,
}

impl Client {
    fn install(self, venv: &TempDir, peryx: &Peryx, spec: &str) {
        match self {
            Self::Pip => pip_install(venv, peryx, spec),
            Self::Uv => uv_install(venv, peryx, spec),
        }
    }

    fn install_fails(self, venv: &TempDir, peryx: &Peryx, spec: &str) {
        match self {
            Self::Pip => pip_install_fails(venv, peryx, spec),
            Self::Uv => uv_install_fails(venv, peryx, spec),
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Pip => "pip",
            Self::Uv => "uv",
        }
    }
}

fn run_against(cmd: &mut Command, what: &str, peryx: &Peryx) {
    let output = cmd.output().expect(what);
    if !output.status.success() {
        eprintln!("=== peryx server log (port {}) ===\n{}", peryx.port, peryx.server_log());
        panic!("{what} failed:\n{}", String::from_utf8_lossy(&output.stderr));
    }
}

fn run_against_fails(cmd: &mut Command, what: &str, peryx: &Peryx) {
    let output = cmd.output().expect(what);
    if output.status.success() {
        eprintln!("=== peryx server log (port {}) ===\n{}", peryx.port, peryx.server_log());
        panic!("{what} succeeded but should have failed");
    }
}

fn uv(venv: &TempDir) -> Command {
    let mut cmd = Command::new("uv");
    cmd.env("UV_CACHE_DIR", venv.path().join("uv-cache"));
    cmd
}

fn run(cmd: &mut Command, what: &str) {
    let output = cmd.output().expect(what);
    assert!(
        output.status.success(),
        "{what} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn e2e_fixture_returns_not_found_for_empty_request() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
    let routes = Routes::new();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let (socket, _) = listener.accept().expect("accept fixture request");
            respond(socket, &routes);
        });
        let mut client = TcpStream::connect(listener.local_addr().expect("fixture address")).expect("connect fixture");
        client.shutdown(Shutdown::Write).expect("finish empty request");
        let mut response = String::new();
        client.read_to_string(&mut response).expect("read fixture response");
        assert!(response.starts_with("HTTP/1.1 404 Not Found\r\n"), "{response}");
        assert!(response.ends_with("\r\n\r\nnot found"), "{response}");
    });
}

#[test]
fn e2e_startup_failure_reports_invalid_config() {
    let panic = std::panic::catch_unwind(|| {
        Peryx::start_against_with_overlay_policy("http://127.0.0.1:9/simple/", "unknown = true\n")
    })
    .err()
    .expect("invalid policy must stop startup");
    let message = panic_message(panic);
    assert!(message.contains("exited during startup"), "{message}");
    assert!(message.contains("unknown field `unknown`"), "{message}");
}

#[rstest]
#[case::expected_success(run_against, "--invalid-e2e-test-option", "test command failed:")]
#[case::expected_failure(run_against_fails, "--version", "test command succeeded but should have failed")]
fn e2e_client_runner_reports_unexpected_status(
    #[case] runner: fn(&mut Command, &str, &Peryx),
    #[case] argument: &str,
    #[case] expected: &str,
) {
    let peryx = Peryx::start_against("http://127.0.0.1:9/simple/");
    let mut command = Command::new(peryx_binary());
    command.arg(argument);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runner(&mut command, "test command", &peryx);
    }))
    .expect_err("unexpected status must panic");
    assert!(panic_message(panic).contains(expected));
}

#[test]
fn e2e_run_reports_command_failure() {
    let mut command = Command::new(peryx_binary());
    command.arg("--invalid-e2e-test-option");
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run(&mut command, "test command");
    }))
    .expect_err("failed command must panic");
    assert!(panic_message(panic).contains("test command failed:"));
}

fn panic_message(panic: Box<dyn std::any::Any + Send>) -> String {
    *panic.downcast::<String>().expect("string panic")
}

fn assert_importable(venv: &TempDir, module: &str) {
    run(
        Command::new(venv_python(venv)).args(["-c", &format!("import {module}")]),
        &format!("import {module}"),
    );
}

#[rstest]
#[case::pip(Client::Pip)]
#[case::uv(Client::Uv)]
fn e2e_client_installs_and_resolves_dependencies(#[case] client: Client) {
    let (_upstream, peryx) = hermetic();
    let venv = uv_venv();
    client.install(&venv, &peryx, "peryxa");
    assert_importable(&venv, "peryxa");
    assert_importable(&venv, "peryxb");
}

#[rstest]
#[case::pip(Client::Pip, "pip")]
#[case::uv(Client::Uv, "uv")]
fn e2e_client_uses_pep658_metadata_fast_path(#[case] client: Client, #[case] expected_name: &str) {
    let (_upstream, peryx) = hermetic();
    let venv = uv_venv();
    client.install(&venv, &peryx, "peryxa");
    let client_name = client.name();
    assert_eq!(client_name, expected_name);
    assert!(
        peryx.metadata_requests() >= 1,
        "{client_name} did not fetch a .metadata sibling through peryx",
    );
}

#[rstest]
#[case::pip(Client::Pip)]
#[case::uv(Client::Uv)]
fn e2e_client_respects_policy_blocked_dependency(#[case] client: Client) {
    let (_upstream, peryx) = hermetic_with_overlay_policy("block_projects = [\"peryxb\"]\n");
    let venv = uv_venv();
    client.install_fails(&venv, &peryx, "peryxa");
}

#[test]
fn e2e_json_simple_detail_is_pep691_and_pep700() {
    let (_upstream, peryx) = hermetic();
    let (status, body) = http_get(peryx.port, "/root/pypi/simple/peryxa/").expect("detail");
    assert_eq!(status, 200);
    let json: serde_json::Value = serde_json::from_str(&body).expect("PEP 691 JSON");
    assert_eq!(json["meta"]["api-version"], "1.4");
    let file = &json["files"][0];
    assert!(
        file["url"]
            .as_str()
            .is_some_and(|url| url.contains("/root/pypi/files/")),
        "url not rewritten to peryx"
    );
    assert!(file["size"].is_number(), "PEP 700 size missing");
    assert!(file["hashes"]["sha256"].is_string(), "sha256 hash missing");
    assert!(
        file["core-metadata"]["sha256"].is_string(),
        "PEP 658 core-metadata not advertised"
    );
    assert_eq!(json["versions"][0], "1.0", "PEP 700 versions missing");
}

#[test]
fn e2e_html_simple_detail_is_pep503() {
    let (_upstream, peryx) = hermetic();
    let body = html_get(peryx.port, "/root/pypi/simple/peryxa/");
    assert!(body.contains("<a href="), "no PEP 503 anchors");
    assert!(
        body.contains("data-core-metadata"),
        "PEP 658 attribute not advertised in HTML"
    );
}

#[test]
fn e2e_file_download_is_cached_content_addressed() {
    let (_upstream, peryx) = hermetic();
    let (_, detail) = http_get(peryx.port, "/root/pypi/simple/peryxa/").expect("detail");
    let json: serde_json::Value = serde_json::from_str(&detail).unwrap();
    let path = json["files"][0]["url"].as_str().expect("file url").to_owned();

    let (first, body) = http_get_bytes(peryx.port, &path);
    assert_eq!(first, 200);
    assert!(!body.is_empty(), "empty artifact");
    assert!(body.starts_with(b"PK"), "not a zip/wheel");
    let (second, again) = http_get_bytes(peryx.port, &path);
    assert_eq!(second, 200);
    assert_eq!(body, again, "cached artifact differs from first fetch");
}

fn wheel_on_disk(name: &str) -> (TempDir, PathBuf) {
    wheel_version_on_disk(name, "1.0")
}

fn wheel_version_on_disk(name: &str, version: &str) -> (TempDir, PathBuf) {
    let dist = build_dist(name, version, &[]);
    let dir = TempDir::new().expect("wheel dir");
    let path = dir.path().join(dist.wheel_filename());
    std::fs::write(&path, &dist.wheel).expect("write wheel");
    (dir, path)
}

fn uv_publish(peryx: &Peryx, wheel: &std::path::Path) {
    let mut cmd = Command::new("uv");
    cmd.args(["publish", "--publish-url"])
        .arg(peryx.upload_url())
        .args(["-u", "__token__", "-p", UPLOAD_TOKEN])
        .arg(wheel);
    run(&mut cmd, "uv publish");
}

fn assert_client_fallback_modes(client: Client) {
    for (mode, upstream_allowed) in [("fallback", true), ("private-first", true), ("no-fallback", false)] {
        let policy = format!("fallback_mode = \"{mode}\"\nprotected_names = [\"peryxb\"]\n");
        let (_upstream, peryx) = hermetic_with_overlay_policy(&policy);
        let (_wheel_dir, wheel) = wheel_version_on_disk("peryxa", "2.0");
        uv_publish(&peryx, &wheel);

        let collision_venv = uv_venv();
        client.install(&collision_venv, &peryx, "peryxa");
        assert_importable(&collision_venv, "peryxa");
        let (_, detail) = http_get(peryx.port, "/root/pypi/simple/peryxa/").expect("collision detail");
        assert!(detail.contains("peryxa-2.0-py3-none-any.whl"));
        assert_eq!(detail.contains("peryxa-1.0-py3-none-any.whl"), mode == "fallback");

        let missing_venv = uv_venv();
        if upstream_allowed {
            client.install(&missing_venv, &peryx, "peryxc");
            assert_importable(&missing_venv, "peryxc");
        } else {
            client.install_fails(&missing_venv, &peryx, "peryxc");
        }

        client.install_fails(&uv_venv(), &peryx, "peryxb");
    }
}

#[rstest]
#[case::pip(Client::Pip)]
#[case::uv(Client::Uv)]
fn e2e_client_respects_virtual_fallback_modes(#[case] client: Client) {
    assert_client_fallback_modes(client);
}

#[test]
fn e2e_twine_upload_then_install() {
    let peryx = Peryx::start_against("http://127.0.0.1:9/simple/");
    let (_dir, wheel) = wheel_on_disk("peryxtwine");
    let mut cmd = Command::new("twine");
    cmd.args([
        "upload",
        "--non-interactive",
        "--disable-progress-bar",
        "--repository-url",
    ])
    .arg(peryx.upload_url())
    .args(["-u", "__token__", "-p", UPLOAD_TOKEN])
    .arg(&wheel);
    run(&mut cmd, "twine upload");

    let venv = uv_venv();
    uv_install(&venv, &peryx, "peryxtwine");
    assert_importable(&venv, "peryxtwine");
}

#[test]
fn e2e_uv_publish_then_install() {
    let peryx = Peryx::start_against("http://127.0.0.1:9/simple/");
    let (_dir, wheel) = wheel_on_disk("peryxpublish");
    uv_publish(&peryx, &wheel);

    let venv = uv_venv();
    uv_install(&venv, &peryx, "peryxpublish");
    assert_importable(&venv, "peryxpublish");
}

#[test]
fn e2e_yank_and_delete_round_trip() {
    let peryx = Peryx::start_against("http://127.0.0.1:9/simple/");
    let (_dir, wheel) = wheel_on_disk("peryxremove");
    uv_publish(&peryx, &wheel);

    assert_http_status(&peryx, "PUT", "/root/pypi/peryxremove/1.0/yank", 200);
    let (_, yanked) = http_get(peryx.port, "/root/pypi/simple/peryxremove/").expect("detail");
    assert!(yanked.contains("\"yanked\":true"), "yank marker missing");

    assert_http_status(&peryx, "DELETE", "/root/pypi/peryxremove/1.0/yank", 200);
    let (_, restored) = http_get(peryx.port, "/root/pypi/simple/peryxremove/").expect("detail");
    assert!(!restored.contains("\"yanked\":true"), "yank marker not cleared");

    assert_http_status(&peryx, "DELETE", "/root/pypi/peryxremove/", 200);
    let (status, _) = http_get(peryx.port, "/root/pypi/simple/peryxremove/").expect("detail");
    assert_eq!(status, 404, "project still served after delete");
}

#[test]
fn e2e_published_project_is_visible_through_the_virtual_index() {
    let peryx = Peryx::start_against("http://127.0.0.1:9/simple/");
    let (_dir, wheel) = wheel_on_disk("peryxremove");
    uv_publish(&peryx, &wheel);

    let (status, detail) = http_get(peryx.port, "/root/pypi/simple/peryxremove/").expect("detail");
    assert_eq!(status, 200);
    assert!(
        detail.contains("peryxremove-1.0-py3-none-any.whl"),
        "published wheel missing"
    );
    assert!(detail.contains("\"yanked\":false"), "published wheel is not active");
}

fn assert_http_status(peryx: &Peryx, verb: &str, path: &str, expected: u16) {
    let (status, response) = http_verb(peryx.port, verb, path);
    assert_eq!(status, expected, "{response}\n{}", peryx.server_log());
}

fn http_verb(port: u16, verb: &str, path: &str) -> (u16, String) {
    let credentials = STANDARD.encode(format!("__token__:{UPLOAD_TOKEN}").as_bytes());
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .write_all(
            format!(
                "{verb} {path} HTTP/1.0\r\nHost: localhost\r\nAuthorization: Basic {credentials}\r\n\
                 Connection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .expect("write request");
    let mut raw = String::new();
    stream.read_to_string(&mut raw).expect("read response");
    let status = raw
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("status code");
    (status, raw)
}

#[cfg(feature = "e2e-live")]
#[rstest]
#[case::pip(Client::Pip)]
#[case::uv(Client::Uv)]
fn e2e_live_client_installs_from_pypi_via_pep658(#[case] client: Client) {
    let peryx = Peryx::start_against("https://pypi.org/simple/");
    let venv = uv_venv();
    client.install(&venv, &peryx, "certifi");
    assert_importable(&venv, "certifi");
    assert!(
        peryx.metadata_requests() >= 1,
        "{} did not use PEP 658 against live pypi",
        client.name(),
    );
}

#[test]
fn e2e_web_ui_dashboard_and_project_page() {
    let (_upstream, peryx) = hermetic();
    let (status, dashboard) = http_get(peryx.port, "/").expect("dashboard");
    assert_eq!(status, 200);
    assert!(dashboard.contains("change serial"), "dashboard stats missing");
    assert!(dashboard.contains("root/pypi"), "index card missing");
    assert!(dashboard.contains("/pkg/peryx_web.js"), "hydration script missing");

    let (status, page) = http_get(peryx.port, "/browse?index=root%2Fpypi&project=peryxa").expect("project page");
    assert_eq!(status, 200);
    assert!(page.contains("peryxa"), "project heading missing");
    assert!(page.contains("<summary>Manage</summary>"), "admin panel missing");
}

#[test]
fn e2e_upstream_yank_hide_restore_round_trip() {
    let (_upstream, peryx) = hermetic();

    let (_, detail) = http_get(peryx.port, "/root/pypi/simple/peryxa/").expect("detail");
    assert!(detail.contains("peryxa-1.0-py3-none-any.whl"));

    assert_http_status(&peryx, "PUT", "/root/pypi/peryxa/1.0/yank", 200);
    let (_, yanked) = http_get(peryx.port, "/root/pypi/simple/peryxa/").expect("detail");
    assert!(
        yanked.contains("\"yanked\":true"),
        "upstream file not yanked via virtual index"
    );
    assert_http_status(&peryx, "DELETE", "/root/pypi/peryxa/1.0/yank", 200);

    assert_http_status(&peryx, "DELETE", "/root/pypi/peryxa/", 200);
    let (_, hidden) = http_get(peryx.port, "/root/pypi/simple/peryxa/").expect("detail");
    assert!(
        !hidden.contains("peryxa-1.0-py3-none-any.whl"),
        "file still served after delete"
    );
    assert_http_status(&peryx, "PUT", "/root/pypi/peryxa/restore", 200);
    let (_, restored) = http_get(peryx.port, "/root/pypi/simple/peryxa/").expect("detail");
    assert!(restored.contains("peryxa-1.0-py3-none-any.whl"), "file not restored");
}

#[test]
fn e2e_inspect_uploaded_wheel() {
    let peryx = Peryx::start_against("http://127.0.0.1:9/simple/");
    let (_dir, wheel) = wheel_on_disk("peryxinspect");
    uv_publish(&peryx, &wheel);

    let (_, detail) = http_get(peryx.port, "/root/pypi/simple/peryxinspect/").expect("detail");
    let sha = detail
        .split("files/")
        .nth(1)
        .expect("file url")
        .split('/')
        .next()
        .expect("sha")
        .to_owned();
    let listing_url = format!("/root/pypi/inspect/{sha}/peryxinspect-1.0-py3-none-any.whl");
    let (status, listing) = http_get(peryx.port, &listing_url).expect("listing");
    assert_eq!(status, 200);
    assert!(listing.contains("dist-info/METADATA"));
    let (status, member) = http_get(
        peryx.port,
        &format!("{listing_url}/peryxinspect-1.0.dist-info/METADATA"),
    )
    .expect("member");
    assert_eq!(status, 200);
    assert!(member.contains("Metadata-Version: 2.1"));
}
