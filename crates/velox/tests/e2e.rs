//! End-to-end regression tests: real pip/uv/twine driven against a spawned velox binary, proving
//! downstream clients work through velox for real.
//!
//! Gated behind the `e2e` feature so they never run in the default `cargo test` or the coverage gate
//! (they need the clients and are slower than unit tests). Two tiers:
//!
//! - **`e2e` (hermetic)**: velox proxies a local fixture index that serves a couple of tiny, real,
//!   installable wheels. No external network, so it is deterministic, flake-free, and fast — the
//!   fixed cost is velox spawn (~0.1s) plus in-process fetches, not a pypi.org round trip. Run with
//!   `cargo test -p velox --features e2e`.
//! - **`e2e-live`**: the same client flows against the real pypi.org, to catch upstream drift. Run
//!   with `cargo test -p velox --features e2e-live` in a network-enabled job.
//!
//! Design goals, per the project's testing philosophy:
//! - **Isolated**: every test owns its own velox server (own temp data dir, own ephemeral port) and,
//!   for hermetic tests, its own fixture upstream. No shared cache or counter state; any test runs
//!   alone.
//! - **Parallel**: because state is per-test, the default multi-threaded runner just works.
//! - **Proof, not assumption**: the PEP 658 fast path is asserted from velox's own `/metrics`
//!   counter — observed at the server, not inferred from the client exiting 0.
#![cfg(feature = "e2e")]

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{Cursor, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tempfile::TempDir;
use velox_storage::blob::Digest;
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

const SIMPLE_JSON_CT: &str = "application/vnd.pypi.simple.v1+json";

/// The upload token every spawned velox is configured with, so twine and `uv publish` can push to
/// the private `root/local` index.
const UPLOAD_TOKEN: &str = "e2e-upload-secret";

/// A minimal but genuinely pip/uv-installable distribution built in memory. `metadata` is both the
/// wheel's `dist-info/METADATA` and the PEP 658 `.metadata` sibling the fixture advertises.
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

/// Build a pure-Python wheel for `name` with the given `Requires-Dist` dependencies. The single
/// module just sets `VALUE`, enough to prove it imported.
fn build_dist(name: &str, version: &str, requires: &[&str]) -> Dist {
    let dist_info = format!("{name}-{version}.dist-info");
    let mut metadata = format!("Metadata-Version: 2.1\nName: {name}\nVersion: {version}\nRequires-Python: >=3.8\n");
    for dep in requires {
        writeln!(metadata, "Requires-Dist: {dep}").expect("write metadata");
    }
    let wheel_meta = "Wheel-Version: 1.0\nGenerator: velox-e2e\nRoot-Is-Purelib: true\nTag: py3-none-any\n";
    let record = format!("{name}/__init__.py,,\n{dist_info}/METADATA,,\n{dist_info}/WHEEL,,\n{dist_info}/RECORD,,\n");
    let init = format!("VALUE = {name:?}\n");
    let mut buf = Vec::new();
    {
        // Entries borrow their contents; only the zip's compressed output is allocated. `metadata`
        // is then moved (not copied) into the Dist to double as the PEP 658 sibling.
        let entries: [(String, &[u8]); 4] = [
            (format!("{name}/__init__.py"), init.as_bytes()),
            (format!("{dist_info}/METADATA"), metadata.as_bytes()),
            (format!("{dist_info}/WHEEL"), wheel_meta.as_bytes()),
            (format!("{dist_info}/RECORD"), record.as_bytes()),
        ];
        let mut zip = zip::ZipWriter::new(Cursor::new(&mut buf));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (path, content) in &entries {
            zip.start_file(path.as_str(), options).expect("zip entry");
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

/// The PEP 691 detail page the fixture serves for a distribution, advertising a content-addressed
/// wheel and its PEP 658 `.metadata` sibling with the sha256s velox will verify against.
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

/// A local HTTP index velox proxies as its upstream. Serves `veloxa` (which requires `veloxb`) and
/// `veloxb`, so dependency resolution, downloads, and PEP 658 metadata all exercise real client
/// behavior with no external network. Dropping it stops the server thread.
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
            build_dist("veloxa", "1.0", &["veloxb"]),
            build_dist("veloxb", "1.0", &[]),
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
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Accept loop: non-blocking so it can notice the stop flag, one thread per connection.
fn serve(listener: &TcpListener, routes: &Arc<Routes>, stop: &Arc<AtomicBool>) {
    listener.set_nonblocking(true).expect("nonblocking");
    while !stop.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((socket, _)) => {
                let routes = Arc::clone(routes);
                std::thread::spawn(move || respond(socket, &routes));
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(2)),
            Err(_) => break,
        }
    }
}

/// Read one HTTP request, route by path, and write a `Connection: close` response.
fn respond(mut socket: TcpStream, routes: &Routes) {
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
    // Body borrows straight from the route map — the wheel bytes are never copied per request.
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

/// A velox process bound to a free loopback port, with its data directory in a temp dir. Dropping it
/// kills the child and removes the data dir, so tests leak nothing.
struct Velox {
    child: Child,
    port: u16,
    _data: TempDir,
}

impl Velox {
    /// Spawn velox proxying the given upstream (a fixture, or pypi.org) and wait until it answers.
    /// Configuration is a TOML file (the only config surface besides the operational flags).
    fn start_against(upstream_url: &str) -> Self {
        let port = free_port();
        let data = TempDir::new().expect("temp data dir");
        let config = data.path().join("velox.toml");
        std::fs::write(&config, format!("upstream_url = \"{upstream_url}\"\nupload_token = \"{UPLOAD_TOKEN}\"\n"))
            .expect("write config");
        let child = Command::new(env!("CARGO_BIN_EXE_velox"))
            .args(["--port", &port.to_string()])
            .arg("--data-dir")
            .arg(data.path())
            .arg("--config")
            .arg(&config)
            .arg("serve")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn velox");
        let velox = Self {
            child,
            port,
            _data: data,
        };
        velox.wait_ready();
        velox
    }

    fn wait_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if let Some((status, _)) = http_get(self.port, "/+status") {
                assert_eq!(status, 200, "unexpected /+status");
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("velox did not become ready on port {}", self.port);
    }

    /// The client-facing simple index URL for the built-in `root/pypi` mirror.
    fn index_url(&self) -> String {
        format!("http://127.0.0.1:{}/root/pypi/simple/", self.port)
    }

    /// The upload endpoint (repository URL) of the private `root/local` index.
    fn upload_url(&self) -> String {
        format!("http://127.0.0.1:{}/root/local/", self.port)
    }

    /// The simple index URL of the private `root/local` index, to install what was uploaded.
    fn local_index_url(&self) -> String {
        format!("http://127.0.0.1:{}/root/local/simple/", self.port)
    }

    /// Read velox's `velox_metadata_requests_total` counter — the PEP 658 siblings it has served.
    fn metadata_requests(&self) -> u64 {
        let (status, body) = http_get(self.port, "/metrics").expect("metrics");
        assert_eq!(status, 200);
        parse_counter(&body, "velox_metadata_requests_total")
    }
}

impl Drop for Velox {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Stand up a hermetic fixture upstream and a velox proxying it. Both live until the tuple drops.
fn hermetic() -> (Upstream, Velox) {
    let upstream = Upstream::start();
    let velox = Velox::start_against(&upstream.upstream_url());
    (upstream, velox)
}

/// Grab a free loopback port by binding to `:0` and releasing it. A spawned server re-binds it a
/// moment later; the window is tiny and each test uses a distinct port, so parallel runs don't clash.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind ephemeral")
        .local_addr()
        .expect("local addr")
        .port()
}

/// Minimal dependency-free HTTP/1.0 GET asking for JSON. Returns `(status, body)`, or `None` if the
/// connection is refused (server not up yet). Panics only on a mid-stream I/O error.
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

/// Like [`http_get`] but returns the raw body bytes, for binary artifacts that are not UTF-8.
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

/// A raw GET that asks for HTML (velox negotiates on Accept), returning the body.
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

/// Pull the value off a Prometheus `# TYPE ... counter` line like `name 3`.
fn parse_counter(metrics: &str, name: &str) -> u64 {
    metrics
        .lines()
        .find_map(|line| line.strip_prefix(name)?.trim().parse().ok())
        .unwrap_or_else(|| panic!("metric {name} not found"))
}

/// Create an isolated, empty virtualenv with `uv venv` (~15ms, no seed packages). Both clients
/// install into it by pointing at its interpreter — no activation, nothing shared between tests.
fn uv_venv() -> TempDir {
    let dir = TempDir::new().expect("venv dir");
    run(Command::new("uv").arg("venv").arg(dir.path()), "uv venv");
    dir
}

fn venv_python(venv: &TempDir) -> PathBuf {
    venv.path().join("bin").join("python")
}

/// Install into `venv` with the real pip client. `pip --python <interp>` targets the venv without
/// seeding pip into it (faster) and without activation.
fn pip_install(venv: &TempDir, index_url: &str, spec: &str) {
    let mut cmd = Command::new("pip3");
    cmd.arg("--python").arg(venv_python(venv)).args([
        "install",
        "--no-cache-dir",
        "--no-input",
        "--index-url",
        index_url,
        spec,
    ]);
    run(&mut cmd, "pip install");
}

/// Install into `venv` with uv targeting that interpreter — faster than pip, still isolated.
fn uv_install(venv: &TempDir, index_url: &str, spec: &str) {
    let mut cmd = Command::new("uv");
    cmd.args(["pip", "install", "--python"])
        .arg(venv_python(venv))
        .args(["--index-url", index_url, spec]);
    run(&mut cmd, "uv pip install");
}

/// Run a command, surfacing captured stderr if it fails.
fn run(cmd: &mut Command, what: &str) {
    let output = cmd.output().unwrap_or_else(|err| panic!("spawn {what}: {err}"));
    assert!(
        output.status.success(),
        "{what} failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// The real proof a distribution installed and works: import it with the venv's interpreter.
fn assert_importable(venv: &TempDir, module: &str) {
    run(
        Command::new(venv_python(venv)).args(["-c", &format!("import {module}")]),
        &format!("import {module}"),
    );
}

#[test]
fn e2e_pip_installs_and_resolves_dependencies() {
    let (_upstream, velox) = hermetic();
    let venv = uv_venv();
    pip_install(&venv, &velox.index_url(), "veloxa");
    assert_importable(&venv, "veloxa");
    assert_importable(&venv, "veloxb"); // transitive dependency resolved through velox
}

#[test]
fn e2e_pip_uses_pep658_metadata_fast_path() {
    let (_upstream, velox) = hermetic();
    let venv = uv_venv();
    pip_install(&venv, &velox.index_url(), "veloxa");
    assert!(
        velox.metadata_requests() >= 1,
        "pip did not fetch a .metadata sibling through velox"
    );
}

#[test]
fn e2e_uv_installs_and_resolves_dependencies() {
    let (_upstream, velox) = hermetic();
    let venv = uv_venv();
    uv_install(&venv, &velox.index_url(), "veloxa");
    assert_importable(&venv, "veloxa");
    assert_importable(&venv, "veloxb"); // transitive dependency resolved through velox
}

#[test]
fn e2e_uv_uses_pep658_metadata_fast_path() {
    let (_upstream, velox) = hermetic();
    let venv = uv_venv();
    uv_install(&venv, &velox.index_url(), "veloxa");
    assert!(
        velox.metadata_requests() >= 1,
        "uv did not fetch a .metadata sibling through velox"
    );
}

#[test]
fn e2e_json_simple_detail_is_pep691_and_pep700() {
    let (_upstream, velox) = hermetic();
    let (status, body) = http_get(velox.port, "/root/pypi/simple/veloxa/").expect("detail");
    assert_eq!(status, 200);
    let json: serde_json::Value = serde_json::from_str(&body).expect("PEP 691 JSON");
    assert_eq!(json["meta"]["api-version"], "1.1");
    let file = &json["files"][0];
    assert!(
        file["url"]
            .as_str()
            .is_some_and(|url| url.contains("/root/pypi/files/")),
        "url not rewritten to velox"
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
    let (_upstream, velox) = hermetic();
    let body = html_get(velox.port, "/root/pypi/simple/veloxa/");
    assert!(body.contains("<a href="), "no PEP 503 anchors");
    assert!(
        body.contains("data-core-metadata"),
        "PEP 658 attribute not advertised in HTML"
    );
}

#[test]
fn e2e_file_download_is_cached_content_addressed() {
    let (_upstream, velox) = hermetic();
    let (_, detail) = http_get(velox.port, "/root/pypi/simple/veloxa/").expect("detail");
    let json: serde_json::Value = serde_json::from_str(&detail).unwrap();
    let path = json["files"][0]["url"].as_str().expect("file url").to_owned();

    let (first, body) = http_get_bytes(velox.port, &path);
    assert_eq!(first, 200);
    assert!(!body.is_empty(), "empty artifact");
    assert!(body.starts_with(b"PK"), "not a zip/wheel");
    let (second, again) = http_get_bytes(velox.port, &path);
    assert_eq!(second, 200);
    assert_eq!(body, again, "cached artifact differs from first fetch");
}

/// Write a built distribution's wheel to a temp file, returning the dir (kept alive) and the path.
fn wheel_on_disk(name: &str) -> (TempDir, PathBuf) {
    let dist = build_dist(name, "1.0", &[]);
    let dir = TempDir::new().expect("wheel dir");
    let path = dir.path().join(dist.wheel_filename());
    std::fs::write(&path, &dist.wheel).expect("write wheel");
    (dir, path)
}

#[test]
fn e2e_twine_upload_then_install() {
    let velox = Velox::start_against("http://127.0.0.1:9/simple/");
    let (_dir, wheel) = wheel_on_disk("veloxtwine");
    let mut cmd = Command::new("twine");
    cmd.args(["upload", "--non-interactive", "--disable-progress-bar", "--repository-url"])
        .arg(velox.upload_url())
        .args(["-u", "__token__", "-p", UPLOAD_TOKEN])
        .arg(&wheel);
    run(&mut cmd, "twine upload");

    let venv = uv_venv();
    uv_install(&venv, &velox.local_index_url(), "veloxtwine");
    assert_importable(&venv, "veloxtwine");
}

#[test]
fn e2e_uv_publish_then_install() {
    let velox = Velox::start_against("http://127.0.0.1:9/simple/");
    let (_dir, wheel) = wheel_on_disk("veloxpublish");
    let mut cmd = Command::new("uv");
    cmd.args(["publish", "--publish-url"])
        .arg(velox.upload_url())
        .args(["-u", "__token__", "-p", UPLOAD_TOKEN])
        .arg(&wheel);
    run(&mut cmd, "uv publish");

    let venv = uv_venv();
    uv_install(&venv, &velox.local_index_url(), "veloxpublish");
    assert_importable(&venv, "veloxpublish");
}

/// The same client flows, but against the real pypi.org, to catch upstream drift.
#[cfg(feature = "e2e-live")]
fn live() -> Velox {
    Velox::start_against("https://pypi.org/simple/")
}

#[cfg(feature = "e2e-live")]
#[test]
fn e2e_live_pip_installs_from_pypi_via_pep658() {
    let velox = live();
    let venv = uv_venv();
    pip_install(&venv, &velox.index_url(), "certifi");
    assert_importable(&venv, "certifi");
    assert!(
        velox.metadata_requests() >= 1,
        "pip did not use PEP 658 against live pypi"
    );
}

#[cfg(feature = "e2e-live")]
#[test]
fn e2e_live_uv_installs_from_pypi_via_pep658() {
    let velox = live();
    let venv = uv_venv();
    uv_install(&venv, &velox.index_url(), "certifi");
    assert_importable(&venv, "certifi");
    assert!(
        velox.metadata_requests() >= 1,
        "uv did not use PEP 658 against live pypi"
    );
}
