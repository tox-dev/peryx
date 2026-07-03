//! End-to-end regression tests: real pip/uv/twine driven against a spawned velodex binary, proving
//! downstream clients work through velodex for real.
//!
//! Gated behind the `e2e` feature so they never run in the default `cargo test` or the coverage gate
//! (they need the clients and are slower than unit tests). Two tiers:
//!
//! - **`e2e` (hermetic)**: velodex proxies a local fixture index that serves a couple of tiny, real,
//!   installable wheels. No external network, so it is deterministic, flake-free, and fast — the
//!   fixed cost is velodex spawn (~0.1s) plus in-process fetches, not a pypi.org round trip. Run with
//!   `cargo test -p velodex --features e2e`.
//! - **`e2e-live`**: the same client flows against the real pypi.org, to catch upstream drift. Run
//!   with `cargo test -p velodex --features e2e-live` in a network-enabled job.
//!
//! Design goals, per the project's testing philosophy:
//! - **Isolated**: every test owns its own velodex server (own temp data dir, own ephemeral port) and,
//!   for hermetic tests, its own fixture upstream. No shared cache or counter state; any test runs
//!   alone.
//! - **Parallel**: because state is per-test, the default multi-threaded runner just works.
//! - **Proof, not assumption**: the PEP 658 fast path is asserted from velodex's own `/metrics`
//!   counter — observed at the server, not inferred from the client exiting 0.
#![cfg(feature = "e2e")]

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{Cursor, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use velodex_storage::blob::Digest;
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

const SIMPLE_JSON_CT: &str = "application/vnd.pypi.simple.v1+json";

/// The upload token every spawned velodex is configured with, so twine and `uv publish` can push to
/// the local layer of the `root/pypi` overlay.
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
    let wheel_meta = "Wheel-Version: 1.0\nGenerator: velodex-e2e\nRoot-Is-Purelib: true\nTag: py3-none-any\n";
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
        // Entries borrow their contents; only the zip's compressed output is allocated. `metadata`
        // is then moved (not copied) into the Dist to double as the PEP 658 sibling.
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

/// The PEP 691 detail page the fixture serves for a distribution, advertising a content-addressed
/// wheel and its PEP 658 `.metadata` sibling with the sha256s velodex will verify against.
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

/// A local HTTP index velodex proxies as its upstream. Serves `velodexa` (which requires `velodexb`) and
/// `velodexb`, so dependency resolution, downloads, and PEP 658 metadata all exercise real client
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
            build_dist("velodexa", "1.0", &["velodexb"]),
            build_dist("velodexb", "1.0", &[]),
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
    // On macOS an accepted socket inherits the listener's non-blocking mode, so reads would return
    // EWOULDBLOCK (seen as EOF here) and writes could truncate. Restore blocking I/O explicitly.
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

/// A velodex process bound to a free loopback port, with its data directory in a temp dir. Dropping it
/// kills the child and removes the data dir, so tests leak nothing.
struct Velodex {
    child: Child,
    port: u16,
    data: TempDir,
}

impl Velodex {
    /// Spawn velodex proxying the given upstream (a fixture, or pypi.org) and wait until it answers.
    /// Configuration is a TOML file (the only config surface besides the operational flags).
    ///
    /// Picking a free port and re-binding it in the child is racy under parallel tests: another
    /// test's velodex can grab the port in the window, our child dies at bind, and the health check
    /// would get answers from the *other* server. Detecting the child's exit and retrying on a fresh
    /// port makes startup deterministic.
    fn start_against(upstream_url: &str) -> Self {
        let data = TempDir::new().expect("temp data dir");
        let config = data.path().join("velodex.toml");
        // A mirror of the given upstream with a local store overlaid in front, served at root/pypi.
        let config_toml = format!(
            "[[index]]\nname = \"upstream\"\nroute = \"upstream\"\nmirror = \"{upstream_url}\"\n\
             [[index]]\nname = \"local\"\nlocal = true\nupload_token = \"{UPLOAD_TOKEN}\"\n\
             [[index]]\nname = \"root/pypi\"\nroute = \"root/pypi\"\nlayers = [\"local\", \"upstream\"]\nupload = \"local\"\n"
        );
        std::fs::write(&config, config_toml).expect("write config");
        for attempt in 0..10 {
            let port = free_port();
            // The server's own log lands next to its data, so a failing test can be diagnosed from
            // what velodex saw rather than only what the client printed.
            let log = std::fs::File::create(data.path().join("velodex.log")).expect("create server log");
            let mut child = Command::new(env!("CARGO_BIN_EXE_velodex"))
                .arg("serve")
                .args(["--port", &port.to_string()])
                .arg("--data-dir")
                .arg(data.path())
                .arg("--config")
                .arg(&config)
                .args(["--log-level", "debug"])
                .stdout(log.try_clone().expect("clone log handle"))
                .stderr(log)
                .spawn()
                .expect("spawn velodex");
            if wait_ready(&mut child, port) {
                return Self { child, port, data };
            }
            let _ = child.wait(); // exited at bind; reap and retry on a fresh port
            eprintln!("velodex lost the race for port {port} (attempt {attempt}), retrying on a fresh port");
        }
        panic!("velodex failed to bind a free port after 10 attempts");
    }

    /// The tail of the server's own log, for failure diagnostics.
    fn server_log(&self) -> String {
        std::fs::read_to_string(self.data.path().join("velodex.log")).unwrap_or_default()
    }

    /// The client-facing simple index URL for the built-in `root/pypi` mirror.
    fn index_url(&self) -> String {
        format!("http://127.0.0.1:{}/root/pypi/simple/", self.port)
    }

    /// The upload endpoint (repository URL): uploads to the root/pypi overlay land in its local layer.
    fn upload_url(&self) -> String {
        format!("http://127.0.0.1:{}/root/pypi/", self.port)
    }

    /// Read velodex's `velodex_metadata_requests_total` counter — the PEP 658 siblings it has served.
    fn metadata_requests(&self) -> u64 {
        let (status, body) = http_get(self.port, "/metrics").expect("metrics");
        assert_eq!(status, 200);
        parse_counter(&body, "velodex_metadata_requests_total")
    }
}

impl Drop for Velodex {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Poll until this spawn's own child answers `/+status`. Returns `false` if the child exited (it
/// lost the port race to another test's server), so the caller can retry on a fresh port.
fn wait_ready(child: &mut Child, port: u16) -> bool {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if child.try_wait().expect("child status").is_some() {
            return false;
        }
        if probe_status(port) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("velodex did not become ready on port {port}");
}

/// One tolerant readiness probe: any I/O failure (including a reset from a transient foreign
/// listener during the port-race window) reads as not-ready, and the body must identify velodex.
fn probe_status(port: u16) -> bool {
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    if stream
        .write_all(b"GET /+status HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut raw = String::new();
    if stream.read_to_string(&mut raw).is_err() {
        return false;
    }
    raw.contains(" 200 ") && raw.contains("\"version\"")
}

/// Stand up a hermetic fixture upstream and a velodex proxying it. Both live until the tuple drops.
fn hermetic() -> (Upstream, Velodex) {
    let upstream = Upstream::start();
    let velodex = Velodex::start_against(&upstream.upstream_url());
    (upstream, velodex)
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

/// A raw GET that asks for HTML (velodex negotiates on Accept), returning the body.
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
    // Plain `uv` here: `uv venv` fetches nothing, and pointing UV_CACHE_DIR inside the target would
    // make it non-empty before creation, which uv refuses. The cache override is for installs.
    run(Command::new("uv").arg("venv").arg(dir.path()), "uv venv");
    dir
}

fn venv_python(venv: &TempDir) -> PathBuf {
    venv.path().join("bin").join("python")
}

/// Install into `venv` with the real pip client. `pip --python <interp>` targets the venv without
/// seeding pip into it (faster) and without activation.
fn pip_install(venv: &TempDir, velodex: &Velodex, spec: &str) {
    let mut cmd = Command::new("pip3");
    cmd.arg("--python").arg(venv_python(venv)).args([
        "install",
        "--no-cache-dir",
        "--no-input",
        "--index-url",
        &velodex.index_url(),
        spec,
    ]);
    run_against(&mut cmd, "pip install", velodex);
}

/// Install into `venv` with uv targeting that interpreter — faster than pip, still isolated.
fn uv_install(venv: &TempDir, velodex: &Velodex, spec: &str) {
    let mut cmd = uv(venv);
    cmd.args(["pip", "install", "--python"])
        .arg(venv_python(venv))
        .args(["--index-url", &velodex.index_url(), spec]);
    run_against(&mut cmd, "uv pip install", velodex);
}

/// Like [`run`], but a failure also dumps the velodex server's own log before panicking — the temp
/// data dir (and the log in it) is deleted during unwind, so it must be printed eagerly.
fn run_against(cmd: &mut Command, what: &str, velodex: &Velodex) {
    let output = cmd.output().unwrap_or_else(|err| panic!("spawn {what}: {err}"));
    if !output.status.success() {
        eprintln!(
            "=== velodex server log (port {}) ===\n{}",
            velodex.port,
            velodex.server_log()
        );
        panic!("{what} failed:\n{}", String::from_utf8_lossy(&output.stderr));
    }
}

/// A uv command with a per-test cache directory inside `venv`. uv's global cache is shared across
/// parallel tests, so a package another test already resolved would be served from cache and never
/// hit velodex — making the PEP 658 metric assertions flaky. A private cache keeps each test's uv
/// fetching through its own velodex while still caching within the test.
fn uv(venv: &TempDir) -> Command {
    let mut cmd = Command::new("uv");
    cmd.env("UV_CACHE_DIR", venv.path().join("uv-cache"));
    cmd
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
    let (_upstream, velodex) = hermetic();
    let venv = uv_venv();
    pip_install(&venv, &velodex, "velodexa");
    assert_importable(&venv, "velodexa");
    assert_importable(&venv, "velodexb"); // transitive dependency resolved through velodex
}

#[test]
fn e2e_pip_uses_pep658_metadata_fast_path() {
    let (_upstream, velodex) = hermetic();
    let venv = uv_venv();
    pip_install(&venv, &velodex, "velodexa");
    assert!(
        velodex.metadata_requests() >= 1,
        "pip did not fetch a .metadata sibling through velodex"
    );
}

#[test]
fn e2e_uv_installs_and_resolves_dependencies() {
    let (_upstream, velodex) = hermetic();
    let venv = uv_venv();
    uv_install(&venv, &velodex, "velodexa");
    assert_importable(&venv, "velodexa");
    assert_importable(&venv, "velodexb"); // transitive dependency resolved through velodex
}

#[test]
fn e2e_uv_uses_pep658_metadata_fast_path() {
    let (_upstream, velodex) = hermetic();
    let venv = uv_venv();
    uv_install(&venv, &velodex, "velodexa");
    assert!(
        velodex.metadata_requests() >= 1,
        "uv did not fetch a .metadata sibling through velodex"
    );
}

#[test]
fn e2e_json_simple_detail_is_pep691_and_pep700() {
    let (_upstream, velodex) = hermetic();
    let (status, body) = http_get(velodex.port, "/root/pypi/simple/velodexa/").expect("detail");
    assert_eq!(status, 200);
    let json: serde_json::Value = serde_json::from_str(&body).expect("PEP 691 JSON");
    assert_eq!(json["meta"]["api-version"], "1.4");
    let file = &json["files"][0];
    assert!(
        file["url"]
            .as_str()
            .is_some_and(|url| url.contains("/root/pypi/files/")),
        "url not rewritten to velodex"
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
    let (_upstream, velodex) = hermetic();
    let body = html_get(velodex.port, "/root/pypi/simple/velodexa/");
    assert!(body.contains("<a href="), "no PEP 503 anchors");
    assert!(
        body.contains("data-core-metadata"),
        "PEP 658 attribute not advertised in HTML"
    );
}

#[test]
fn e2e_file_download_is_cached_content_addressed() {
    let (_upstream, velodex) = hermetic();
    let (_, detail) = http_get(velodex.port, "/root/pypi/simple/velodexa/").expect("detail");
    let json: serde_json::Value = serde_json::from_str(&detail).unwrap();
    let path = json["files"][0]["url"].as_str().expect("file url").to_owned();

    let (first, body) = http_get_bytes(velodex.port, &path);
    assert_eq!(first, 200);
    assert!(!body.is_empty(), "empty artifact");
    assert!(body.starts_with(b"PK"), "not a zip/wheel");
    let (second, again) = http_get_bytes(velodex.port, &path);
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
    let velodex = Velodex::start_against("http://127.0.0.1:9/simple/");
    let (_dir, wheel) = wheel_on_disk("velodextwine");
    let mut cmd = Command::new("twine");
    cmd.args([
        "upload",
        "--non-interactive",
        "--disable-progress-bar",
        "--repository-url",
    ])
    .arg(velodex.upload_url())
    .args(["-u", "__token__", "-p", UPLOAD_TOKEN])
    .arg(&wheel);
    run(&mut cmd, "twine upload");

    let venv = uv_venv();
    uv_install(&venv, &velodex, "velodextwine");
    assert_importable(&venv, "velodextwine");
}

#[test]
fn e2e_uv_publish_then_install() {
    let velodex = Velodex::start_against("http://127.0.0.1:9/simple/");
    let (_dir, wheel) = wheel_on_disk("velodexpublish");
    let mut cmd = Command::new("uv");
    cmd.args(["publish", "--publish-url"])
        .arg(velodex.upload_url())
        .args(["-u", "__token__", "-p", UPLOAD_TOKEN])
        .arg(&wheel);
    run(&mut cmd, "uv publish");

    let venv = uv_venv();
    uv_install(&venv, &velodex, "velodexpublish");
    assert_importable(&venv, "velodexpublish");
}

#[test]
fn e2e_yank_and_delete_round_trip() {
    let velodex = Velodex::start_against("http://127.0.0.1:9/simple/");
    let (_dir, wheel) = wheel_on_disk("velodexremove");
    let mut cmd = Command::new("uv");
    cmd.args(["publish", "--publish-url"])
        .arg(velodex.upload_url())
        .args(["-u", "__token__", "-p", UPLOAD_TOKEN])
        .arg(&wheel);
    run(&mut cmd, "uv publish");

    // Yank the version: the file stays but carries the PEP 592 marker.
    assert_eq!(http_verb(velodex.port, "PUT", "/root/pypi/velodexremove/1.0/yank"), 200);
    let (_, yanked) = http_get(velodex.port, "/root/pypi/simple/velodexremove/").expect("detail");
    assert!(yanked.contains("\"yanked\":true"), "yank marker missing");

    // Un-yank restores it.
    assert_eq!(
        http_verb(velodex.port, "DELETE", "/root/pypi/velodexremove/1.0/yank"),
        200
    );
    let (_, restored) = http_get(velodex.port, "/root/pypi/simple/velodexremove/").expect("detail");
    assert!(!restored.contains("\"yanked\":true"), "yank marker not cleared");

    // Delete removes the project outright (the local layer is volatile by default).
    assert_eq!(http_verb(velodex.port, "DELETE", "/root/pypi/velodexremove/"), 200);
    let (status, _) = http_get(velodex.port, "/root/pypi/simple/velodexremove/").expect("detail");
    assert_eq!(status, 404, "project still served after delete");
}

/// A raw authenticated HTTP request with no body, as `curl -X <verb> -u __token__:<token>` sends.
fn http_verb(port: u16, verb: &str, path: &str) -> u16 {
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
    raw.split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("status code")
}

/// The same client flows, but against the real pypi.org, to catch upstream drift.
#[cfg(feature = "e2e-live")]
fn live() -> Velodex {
    Velodex::start_against("https://pypi.org/simple/")
}

#[cfg(feature = "e2e-live")]
#[test]
fn e2e_live_pip_installs_from_pypi_via_pep658() {
    let velodex = live();
    let venv = uv_venv();
    pip_install(&venv, &velodex, "certifi");
    assert_importable(&venv, "certifi");
    assert!(
        velodex.metadata_requests() >= 1,
        "pip did not use PEP 658 against live pypi"
    );
}

#[cfg(feature = "e2e-live")]
#[test]
fn e2e_live_uv_installs_from_pypi_via_pep658() {
    let velodex = live();
    let venv = uv_venv();
    uv_install(&venv, &velodex, "certifi");
    assert_importable(&venv, "certifi");
    assert!(
        velodex.metadata_requests() >= 1,
        "uv did not use PEP 658 against live pypi"
    );
}

#[test]
fn e2e_web_ui_dashboard_and_project_page() {
    let (_upstream, velodex) = hermetic();
    let (status, dashboard) = http_get(velodex.port, "/").expect("dashboard");
    assert_eq!(status, 200);
    assert!(dashboard.contains("change serial"), "dashboard stats missing");
    assert!(dashboard.contains("root/pypi"), "index card missing");
    assert!(dashboard.contains("/pkg/velodex_web.js"), "hydration script missing");

    let (status, page) = http_get(velodex.port, "/browse?index=root%2Fpypi&project=velodexa").expect("project page");
    assert_eq!(status, 200);
    assert!(page.contains("velodexa"), "project heading missing");
    assert!(page.contains("Manage uploads"), "admin panel missing");
}

#[test]
fn e2e_upstream_yank_hide_restore_round_trip() {
    let (_upstream, velodex) = hermetic();
    // Warm the overlay so the mirror file is known.
    let (_, detail) = http_get(velodex.port, "/root/pypi/simple/velodexa/").expect("detail");
    assert!(detail.contains("velodexa-1.0-py3-none-any.whl"));

    // Yank the upstream release through the overlay, then clear it.
    assert_eq!(http_verb(velodex.port, "PUT", "/root/pypi/velodexa/1.0/yank"), 200);
    let (_, yanked) = http_get(velodex.port, "/root/pypi/simple/velodexa/").expect("detail");
    assert!(
        yanked.contains("\"yanked\":true"),
        "upstream file not yanked via overlay"
    );
    assert_eq!(http_verb(velodex.port, "DELETE", "/root/pypi/velodexa/1.0/yank"), 200);

    // Hide it outright, confirm it vanished, then restore it.
    assert_eq!(http_verb(velodex.port, "DELETE", "/root/pypi/velodexa/"), 200);
    let (_, hidden) = http_get(velodex.port, "/root/pypi/simple/velodexa/").expect("detail");
    assert!(
        !hidden.contains("velodexa-1.0-py3-none-any.whl"),
        "file still served after delete"
    );
    assert_eq!(http_verb(velodex.port, "PUT", "/root/pypi/velodexa/restore"), 200);
    let (_, restored) = http_get(velodex.port, "/root/pypi/simple/velodexa/").expect("detail");
    assert!(restored.contains("velodexa-1.0-py3-none-any.whl"), "file not restored");
}

#[test]
fn e2e_inspect_uploaded_wheel() {
    let velodex = Velodex::start_against("http://127.0.0.1:9/simple/");
    let (_dir, wheel) = wheel_on_disk("velodexinspect");
    let mut cmd = Command::new("uv");
    cmd.args(["publish", "--publish-url"])
        .arg(velodex.upload_url())
        .args(["-u", "__token__", "-p", UPLOAD_TOKEN])
        .arg(&wheel);
    run(&mut cmd, "uv publish");

    let (_, detail) = http_get(velodex.port, "/root/pypi/simple/velodexinspect/").expect("detail");
    let sha = detail
        .split("files/")
        .nth(1)
        .expect("file url")
        .split('/')
        .next()
        .expect("sha")
        .to_owned();
    let listing_url = format!("/root/pypi/inspect/{sha}/velodexinspect-1.0-py3-none-any.whl");
    let (status, listing) = http_get(velodex.port, &listing_url).expect("listing");
    assert_eq!(status, 200);
    assert!(listing.contains("dist-info/METADATA"));
    let (status, member) = http_get(
        velodex.port,
        &format!("{listing_url}/velodexinspect-1.0.dist-info/METADATA"),
    )
    .expect("member");
    assert_eq!(status, 200);
    assert!(member.contains("Metadata-Version: 2.1"));
}
