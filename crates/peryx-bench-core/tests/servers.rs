use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use peryx_bench_core::servers::{Server, StartupPolicy, http_client};

fn base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/")
}

fn probe(url: &str) -> String {
    url.to_owned()
}

fn python_server(port: u16, state: &Path) -> Command {
    let mut command = Command::new("python3");
    command
        .args(["-m", "http.server", &port.to_string(), "--bind", "127.0.0.1"])
        .current_dir(state);
    command
}

fn exit_early(_: u16, _: &Path) -> Command {
    let mut command = Command::new("sh");
    command.args(["-c", "printf 'startup failed' >&2; exit 7"]);
    command
}

fn never_ready(_: u16, _: &Path) -> Command {
    let mut command = Command::new("sh");
    command.args(["-c", "sleep 2"]);
    command
}

fn missing_command(_: u16, _: &Path) -> Command {
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

fn server(command: Option<fn(u16, &Path) -> Command>) -> Server {
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
        .start(directory.path(), &http_client().expect("HTTP client builds"))
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
    std::fs::write(directory.path().join("index.html"), "ready").expect("fixture writes");
    let client = http_client().expect("HTTP client builds");
    let active = server(Some(python_server))
        .start(directory.path(), &client)
        .await
        .expect("HTTP server starts");
    assert_eq!(
        client
            .get(&active.url)
            .send()
            .await
            .expect("request succeeds")
            .text()
            .await
            .unwrap(),
        "ready"
    );
}

#[tokio::test]
async fn server_reports_early_exit_and_log() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let error = server(Some(exit_early))
        .start_with_policy(
            directory.path(),
            &http_client().expect("HTTP client builds"),
            short_policy(),
        )
        .await
        .err()
        .expect("early exit fails");
    assert!(error.to_string().contains("startup failed"), "{error:#}");
}

#[tokio::test]
async fn server_reports_readiness_timeout() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let error = server(Some(never_ready))
        .start_with_policy(
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
    std::fs::write(directory.path().join("index.html"), "ready").expect("fixture writes");
    let mut fixture = server(Some(python_server));
    fixture.setup = Some(setup);
    fixture.teardown = Some(teardown);
    let active = fixture
        .start(directory.path(), &http_client().expect("HTTP client builds"))
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
    assert_eq!(TORN_DOWN_PORT.load(Ordering::Relaxed), expected);
}

#[tokio::test]
async fn server_describes_spawn_failure() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let error = server(Some(missing_command))
        .start(directory.path(), &http_client().expect("HTTP client builds"))
        .await
        .err()
        .expect("spawn fails");
    assert!(error.to_string().contains("fixture did not start"), "{error:#}");
}
