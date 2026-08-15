use std::collections::HashMap;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{LazyLock, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use peryx_bench_core::context::BenchmarkContext;
use peryx_bench_core::servers::{Server, StartupPolicy, http_client};

fn base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/")
}

fn probe(url: &str) -> String {
    url.to_owned()
}

fn test_server(_: &BenchmarkContext, port: u16, state: &Path) -> Command {
    let requests = if state.join("setup").exists() { 1 } else { 2 };
    let body = std::fs::read(state.join("index.html")).expect("fixture body exists");
    let listener = std::net::TcpListener::bind(("127.0.0.1", port)).expect("fixture server binds");
    let handle = std::thread::spawn(move || serve_fixture(&listener, &body, requests));
    FIXTURE_THREADS
        .lock()
        .expect("fixture thread registry locks")
        .insert(state.to_path_buf(), FixtureThread { handle });
    long_running_command()
}

struct FixtureThread {
    handle: JoinHandle<()>,
}

static FIXTURE_THREADS: LazyLock<Mutex<HashMap<PathBuf, FixtureThread>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

struct FixtureThreadGuard(PathBuf);

impl FixtureThreadGuard {
    fn new(state: &Path) -> Self {
        Self(state.to_path_buf())
    }
}

impl Drop for FixtureThreadGuard {
    fn drop(&mut self) {
        let thread = FIXTURE_THREADS
            .lock()
            .expect("fixture thread registry locks")
            .remove(&self.0)
            .expect("fixture thread was registered");
        thread.handle.join().expect("fixture thread joins");
    }
}

fn exit_early(_: &BenchmarkContext, _: u16, _: &Path) -> Command {
    let mut command = Command::new("rustc");
    command.arg("--peryx-invalid-argument");
    command
}

fn never_ready(_: &BenchmarkContext, _: u16, _: &Path) -> Command {
    long_running_command()
}

fn long_running_command() -> Command {
    #[cfg(unix)]
    let mut command = {
        let mut command = Command::new("sh");
        command.args(["-c", "read value"]);
        command
    };
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("cmd");
        command.args(["/C", "set /p value="]);
        command
    };
    command.stdin(Stdio::piped());
    command
}

fn missing_command(_: &BenchmarkContext, _: u16, _: &Path) -> Command {
    Command::new("peryx-command-that-does-not-exist")
}

fn setup(port: u16, state: &Path) -> anyhow::Result<()> {
    std::fs::write(state.join("setup"), port.to_string())?;
    Ok(())
}

static TORN_DOWN_PORT: AtomicU16 = AtomicU16::new(0);

fn teardown(port: u16) {
    TORN_DOWN_PORT.store(port, Ordering::Relaxed);
}

fn server(command: Option<fn(&BenchmarkContext, u16, &Path) -> Command>) -> Server {
    Server {
        name: "fixture",
        homepage: "https://example.invalid",
        base_url,
        probe,
        command,
        setup: None,
        teardown: None,
    }
}

fn context(state: &Path) -> BenchmarkContext {
    BenchmarkContext::new(state.join("peryx"), state.join("report.toml"))
}

const fn short_policy() -> StartupPolicy {
    StartupPolicy {
        timeout: Duration::from_millis(100),
        request_timeout: Duration::from_millis(20),
        poll_interval: Duration::from_millis(10),
    }
}

#[tokio::test]
async fn direct_server_has_no_process() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let active = server(None)
        .start(
            &context(directory.path()),
            directory.path(),
            &http_client().expect("HTTP client builds"),
        )
        .await
        .expect("direct server starts");
    assert_eq!(
        (active.url.starts_with("http://127.0.0.1:"), active.pid()),
        (true, None)
    );
}

#[tokio::test]
async fn server_waits_until_http_is_ready() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let fixture_thread = FixtureThreadGuard::new(directory.path());
    std::fs::write(directory.path().join("index.html"), "ready").expect("fixture writes");
    let client = http_client().expect("HTTP client builds");
    let active = server(Some(test_server))
        .start(&context(directory.path()), directory.path(), &client)
        .await
        .expect("HTTP server starts");
    let body = client
        .get(&active.url)
        .send()
        .await
        .expect("request succeeds")
        .text()
        .await
        .unwrap();
    drop(fixture_thread);
    assert_eq!(body, "ready");
}

#[tokio::test]
async fn server_reports_early_exit_and_log() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let error = server(Some(exit_early))
        .start_with_policy(
            &context(directory.path()),
            directory.path(),
            &http_client().expect("HTTP client builds"),
            short_policy(),
        )
        .await
        .err()
        .expect("early exit fails");
    assert!(error.to_string().contains("peryx-invalid-argument"), "{error:#}");
}

#[tokio::test]
async fn server_reports_readiness_timeout() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let error = server(Some(never_ready))
        .start_with_policy(
            &context(directory.path()),
            directory.path(),
            &http_client().expect("HTTP client builds"),
            short_policy(),
        )
        .await
        .err()
        .expect("readiness timeout fails");
    assert!(format!("{error:#}").contains("server never answered"), "{error:#}");
}

#[tokio::test]
async fn server_runs_setup_and_teardown() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let fixture_thread = FixtureThreadGuard::new(directory.path());
    std::fs::write(directory.path().join("index.html"), "ready").expect("fixture writes");
    let mut fixture = server(Some(test_server));
    fixture.setup = Some(setup);
    fixture.teardown = Some(teardown);
    let active = fixture
        .start(
            &context(directory.path()),
            directory.path(),
            &http_client().expect("HTTP client builds"),
        )
        .await
        .expect("server starts");
    let port = active
        .url
        .trim_end_matches('/')
        .rsplit(':')
        .next()
        .expect("URL has port");
    assert_eq!(
        std::fs::read_to_string(directory.path().join("setup")).expect("setup marker exists"),
        port
    );
    let expected: u16 = port.parse().expect("port parses");
    drop(active);
    drop(fixture_thread);
    assert_eq!(TORN_DOWN_PORT.load(Ordering::Relaxed), expected);
}

#[tokio::test]
async fn server_describes_spawn_failure() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let error = server(Some(missing_command))
        .start(
            &context(directory.path()),
            directory.path(),
            &http_client().expect("HTTP client builds"),
        )
        .await
        .err()
        .expect("spawn fails");
    assert!(error.to_string().contains("fixture did not start"), "{error:#}");
}

fn serve_fixture(listener: &std::net::TcpListener, body: &[u8], requests: usize) {
    for stream in listener.incoming().take(requests) {
        let mut stream = stream.expect("fixture accepts a request");
        let mut request = [0; 1024];
        let _ = stream.read(&mut request).expect("fixture reads a request");
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .expect("fixture writes headers");
        stream.write_all(body).expect("fixture writes the body");
    }
}
