use std::fmt::Display;
use std::fs::File;
use std::future::Future;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use futures_util::future::join_all;
use rstest::rstest;
use tempfile::TempDir;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::{
    Auth, CredentialFailure, CredentialScope, ExecCredentialConfig, ExecCredentialConfigError,
    ExecCredentialProviderError, UpstreamClient, UpstreamTls,
};

const VALID_EXPIRY: &str = "2099-01-01T00:00:00Z";
const NORMAL_HELPER_DEADLINE: Duration = Duration::from_mins(5);

struct Helper {
    _directory: TempDir,
    executable: PathBuf,
    executions: PathBuf,
    requests: PathBuf,
    start_event: AsyncFd<File>,
}

struct ProcessSignal {
    fifo: AsyncFd<File>,
    guard: Option<File>,
}

impl Helper {
    fn responding(response: &str) -> Self {
        Self::script(|executions, requests| {
            format!(
                "request=$(/bin/cat)\nprintf '%s' \"$request\" > {}\nprintf x >> {}\nprintf '%s' {}\n",
                shell_quote(requests.display()),
                shell_quote(executions.display()),
                shell_quote(response),
            )
        })
    }

    fn script(body: impl FnOnce(&Path, &Path) -> String) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("credential-helper");
        let executions = directory.path().join("executions");
        let requests = directory.path().join("requests");
        let started_path = directory.path().join("started");
        let start_event = fifo(&started_path);
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\nset -u\nprintf x > {}\n{}",
                shell_quote(started_path.display()),
                body(&executions, &requests),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        Self {
            _directory: directory,
            executable,
            executions,
            requests,
            start_event,
        }
    }

    fn config(&self, failure: CredentialFailure) -> ExecCredentialConfig {
        ExecCredentialConfig::new(
            vec![self.executable.display().to_string()],
            NORMAL_HELPER_DEADLINE,
            Vec::new(),
            failure,
        )
        .unwrap()
    }

    fn execution_count(&self) -> usize {
        std::fs::read(&self.executions).map_or(0, |contents| contents.len())
    }

    fn requests_fifo(&self) -> AsyncFd<File> {
        fifo(&self.requests)
    }

    async fn run_after_start<T>(&self, operation: impl Future<Output = T>) -> T {
        let ((), result) = tokio::join!(self.await_start(), operation);
        result
    }

    async fn await_start(&self) {
        Self::wait(&self.start_event).await;
    }

    async fn wait(fifo: &AsyncFd<File>) {
        let mut signal = [0];
        fifo.async_io(Interest::READABLE, |mut file| file.read_exact(&mut signal))
            .await
            .unwrap();
    }

    fn process_signal(&self) -> ProcessSignal {
        assert!(Command::new("mkfifo").arg(&self.requests).status().unwrap().success());
        let read = rustix::fs::open(
            &self.requests,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
        let guard = rustix::fs::open(
            &self.requests,
            rustix::fs::OFlags::WRONLY | rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
        ProcessSignal {
            fifo: AsyncFd::new(File::from(read)).unwrap(),
            guard: Some(File::from(guard)),
        }
    }

    async fn write(fifo: &AsyncFd<File>, bytes: &[u8]) {
        fifo.async_io(Interest::WRITABLE, |mut file| file.write_all(bytes))
            .await
            .unwrap();
    }
}

fn fifo(path: &Path) -> AsyncFd<File> {
    assert!(Command::new("mkfifo").arg(path).status().unwrap().success());
    AsyncFd::new(File::from(
        rustix::fs::open(
            path,
            rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .unwrap(),
    ))
    .unwrap()
}

impl ProcessSignal {
    async fn read(&self) -> String {
        let mut bytes = [0; 32];
        let length = self
            .fifo
            .async_io(Interest::READABLE, |mut file| file.read(&mut bytes))
            .await
            .unwrap();
        String::from_utf8(bytes[..length].to_vec()).unwrap()
    }

    fn release_guard(&mut self) {
        drop(self.guard.take());
    }
}

fn shell_quote(value: impl Display) -> String {
    format!("'{}'", value.to_string().replace('\'', "'\\''"))
}

fn bearer_response(token: &str) -> String {
    format!(r#"{{"version":1,"expires_at":"{VALID_EXPIRY}","type":"bearer","token":"{token}"}}"#)
}

fn basic_response(username: &str, password: &str) -> String {
    format!(
        r#"{{"version":1,"expires_at":"{VALID_EXPIRY}","type":"basic","username":"{username}","password":"{password}"}}"#
    )
}

fn rotating_helper() -> Helper {
    Helper::script(|executions, _| {
        format!(
            "/bin/cat >/dev/null\nif [ -e {} ]; then\n  response={}\nelse\n  : > {}\n  response={}\nfi\nprintf x >> {}\nprintf '%s' \"$response\"\n",
            shell_quote(executions.display()),
            shell_quote(bearer_response("new")),
            shell_quote(executions.display()),
            shell_quote(bearer_response("old")),
            shell_quote(executions.display()),
        )
    })
}

#[rstest]
#[case::empty(Vec::new(), Duration::from_secs(1), ExecCredentialConfigError::EmptyArgv)]
#[case::relative(
    vec!["helper".to_owned()],
    Duration::from_secs(1),
    ExecCredentialConfigError::RelativeExecutable
)]
#[case::nul(
    vec!["/helper\0argument".to_owned()],
    Duration::from_secs(1),
    ExecCredentialConfigError::ArgumentNul
)]
#[case::zero_timeout(
    vec!["/helper".to_owned()],
    Duration::ZERO,
    ExecCredentialConfigError::Timeout
)]
#[case::submillisecond_timeout(
    vec!["/helper".to_owned()],
    Duration::from_nanos(1),
    ExecCredentialConfigError::Timeout
)]
#[case::long_timeout(
    vec!["/helper".to_owned()],
    Duration::from_secs(301),
    ExecCredentialConfigError::Timeout
)]
fn test_config_rejects_invalid_command_shape(
    #[case] argv: Vec<String>,
    #[case] timeout: Duration,
    #[case] expected: ExecCredentialConfigError,
) {
    let error = ExecCredentialConfig::new(argv, timeout, Vec::new(), CredentialFailure::Fail).unwrap_err();

    assert_eq!(error, expected);
    assert!(error.to_string().starts_with("credential helper"));
}

#[rstest]
#[case::item_count(vec!["/helper".to_owned(); 65])]
#[case::byte_count(vec![format!("/{}", "x".repeat((32 << 10) + 1))])]
fn test_config_bounds_argv(#[case] argv: Vec<String>) {
    let error =
        ExecCredentialConfig::new(argv, Duration::from_secs(1), Vec::new(), CredentialFailure::Fail).unwrap_err();

    assert_eq!(error, ExecCredentialConfigError::ArgvLimit);
}

#[rstest]
#[case::empty("")]
#[case::equals("A=B")]
#[case::nul("A\0B")]
fn test_config_rejects_invalid_environment_name(#[case] name: &str) {
    let error = ExecCredentialConfig::new(
        vec!["/helper".to_owned()],
        Duration::from_secs(1),
        vec![name.to_owned()],
        CredentialFailure::Fail,
    )
    .unwrap_err();

    assert_eq!(error, ExecCredentialConfigError::EnvironmentName);
}

#[test]
fn test_config_bounds_environment_names() {
    let error = ExecCredentialConfig::new(
        vec!["/helper".to_owned()],
        Duration::from_secs(1),
        vec!["NAME".to_owned(); 65],
        CredentialFailure::Fail,
    )
    .unwrap_err();

    assert_eq!(error, ExecCredentialConfigError::EnvironmentLimit);
}

#[test]
fn test_config_exposes_resolved_settings() {
    let config = ExecCredentialConfig::new(
        vec!["/helper".to_owned(), "arg".to_owned()],
        Duration::from_secs(12),
        vec!["HOME".to_owned()],
        CredentialFailure::Anonymous,
    )
    .unwrap();

    assert_eq!(
        (config.argv(), config.timeout(), config.environment(), config.failure()),
        (
            &["/helper".to_owned(), "arg".to_owned()][..],
            Duration::from_secs(12),
            &["HOME".to_owned()][..],
            CredentialFailure::Anonymous,
        )
    );
}

#[test]
fn test_config_debug_redacts_command_and_environment() {
    let config = ExecCredentialConfig::new(
        vec!["/secret/helper".to_owned(), "secret-argument".to_owned()],
        Duration::from_secs(1),
        vec!["SECRET_ENVIRONMENT".to_owned()],
        CredentialFailure::Fail,
    )
    .unwrap();

    let debug = format!("{config:?}");

    assert!(debug.contains("argv_items: 2"));
    assert!(!debug.contains("secret"));
    assert!(!debug.contains("SECRET_ENVIRONMENT"));
}

#[rstest]
#[case::relative("not a URL")]
#[case::hostless("file:///tmp/index")]
fn test_provider_rejects_an_invalid_origin(#[case] upstream: &str) {
    let config = ExecCredentialConfig::new(
        vec!["/helper".to_owned()],
        Duration::from_secs(1),
        Vec::new(),
        CredentialFailure::Fail,
    )
    .unwrap();

    let error = config.provider(upstream, CredentialScope::Read).unwrap_err();

    assert_eq!(error, ExecCredentialProviderError::Origin);
    assert_eq!(
        error.to_string(),
        "credential helper upstream origin is invalid or exceeds its limit"
    );
}

#[tokio::test]
async fn test_helper_returns_basic() {
    let helper = Helper::responding(&basic_response("service", "password"));
    let provider = helper
        .config(CredentialFailure::Fail)
        .provider("https://resources.example/api/", CredentialScope::Read)
        .unwrap();

    let snapshot = helper.run_after_start(provider.credential()).await.unwrap();

    assert_eq!(
        snapshot.auth(),
        &Auth::Basic {
            username: "service".to_owned(),
            password: "password".to_owned(),
        }
    );
}

#[tokio::test]
async fn test_helper_receives_only_the_origin_and_scope() {
    let helper = Helper::responding(&bearer_response("token"));
    let provider = helper
        .config(CredentialFailure::Fail)
        .provider(
            "https://ignored:secret@resources.example:8443/api/?query=ignored",
            CredentialScope::Read,
        )
        .unwrap();

    helper.run_after_start(provider.credential()).await.unwrap();

    assert_eq!(
        std::fs::read_to_string(&helper.requests).unwrap(),
        r#"{"version":1,"origin":"https://resources.example:8443","scope":"read"}"#
    );
}

#[tokio::test]
async fn test_cached_credential_skips_the_helper() {
    let helper = Helper::responding(&bearer_response("token"));
    let provider = helper
        .config(CredentialFailure::Fail)
        .provider("https://resources.example/api/", CredentialScope::Read)
        .unwrap();

    helper.run_after_start(provider.credential()).await.unwrap();
    provider.credential().await.unwrap();

    assert_eq!(helper.execution_count(), 1);
}

#[tokio::test]
async fn test_helper_returns_bearer() {
    let helper = Helper::responding(&bearer_response("token"));
    let provider = helper
        .config(CredentialFailure::Fail)
        .provider("https://resources.example/api/", CredentialScope::Read)
        .unwrap();

    let snapshot = helper.run_after_start(provider.credential()).await.unwrap();

    assert_eq!(snapshot.auth(), &Auth::Bearer("token".to_owned()));
}

#[rstest]
#[case::malformed("not-json", "credential helper returned an invalid response")]
#[case::unknown_field(
    r#"{"version":1,"expires_at":"2099-01-01T00:00:00Z","type":"bearer","token":"token","extra":true}"#,
    "credential helper returned an invalid response"
)]
#[case::version(
    r#"{"version":2,"expires_at":"2099-01-01T00:00:00Z","type":"bearer","token":"token"}"#,
    "credential helper returned an unsupported response version"
)]
#[case::expiry(
    r#"{"version":1,"expires_at":"tomorrow","type":"bearer","token":"token"}"#,
    "credential helper returned an invalid expiry"
)]
#[case::expired(
    r#"{"version":1,"expires_at":"2000-01-01T00:00:00Z","type":"bearer","token":"token"}"#,
    "credential helper returned an expired credential"
)]
#[case::type_name(
    r#"{"version":1,"expires_at":"2099-01-01T00:00:00Z","type":"unknown","token":"token"}"#,
    "credential helper returned an invalid response"
)]
#[tokio::test]
async fn test_helper_rejects_invalid_responses(#[case] response: &str, #[case] message: &str) {
    let helper = Helper::responding(response);
    let provider = helper
        .config(CredentialFailure::Fail)
        .provider("https://resources.example", CredentialScope::Read)
        .unwrap();

    let error = helper.run_after_start(provider.credential()).await.unwrap_err();

    assert_eq!(error.to_string(), message);
}

#[tokio::test]
async fn test_helper_rejects_a_credential_inside_the_expiry_margin() {
    let helper = Helper::script(|ready, response| {
        format!(
            "/bin/cat >/dev/null\nprintf x > {}\nIFS= read -r response < {}\nprintf '%s' \"$response\"\n",
            shell_quote(ready.display()),
            shell_quote(response.display())
        )
    });
    let ready = fifo(&helper.executions);
    let response = helper.requests_fifo();
    let provider = helper
        .config(CredentialFailure::Fail)
        .provider("https://resources.example", CredentialScope::Read)
        .unwrap();

    let ((), credential) = tokio::join!(
        async {
            Helper::wait(&ready).await;
            let expiry = (OffsetDateTime::now_utc() + time::Duration::seconds(15))
                .format(&Rfc3339)
                .unwrap();
            let response_line =
                format!("{{\"version\":1,\"expires_at\":\"{expiry}\",\"type\":\"bearer\",\"token\":\"token\"}}\n");
            Helper::write(&response, response_line.as_bytes()).await;
        },
        provider.credential()
    );
    let error = credential.unwrap_err();

    assert_eq!(
        error.to_string(),
        "credential helper returned a credential inside its expiry margin"
    );
}

#[rstest]
#[case::basic_username(&basic_response("", "password"))]
#[case::basic_password(&basic_response("service", ""))]
#[case::bearer(&bearer_response(""))]
#[tokio::test]
async fn test_helper_rejects_empty_credentials(#[case] response: &str) {
    let helper = Helper::responding(response);
    let provider = helper
        .config(CredentialFailure::Fail)
        .provider("https://resources.example", CredentialScope::Read)
        .unwrap();

    let error = helper.run_after_start(provider.credential()).await.unwrap_err();

    assert_eq!(error.to_string(), "credential helper returned an empty credential");
}

#[tokio::test]
async fn test_helper_anonymous_failure_mode_returns_no_auth() {
    let helper = Helper::responding("invalid");
    let provider = helper
        .config(CredentialFailure::Anonymous)
        .provider("https://resources.example", CredentialScope::Read)
        .unwrap();

    let snapshot = helper.run_after_start(provider.credential()).await.unwrap();

    assert_eq!(snapshot.auth(), &Auth::None);
}

#[tokio::test]
async fn test_helper_nonzero_exit_is_redacted() {
    let helper = Helper::script(|_, _| "printf '%s' secret-output >&2\nexit 7\n".to_owned());
    let config = ExecCredentialConfig::new(
        vec![helper.executable.display().to_string(), "secret-argument".to_owned()],
        NORMAL_HELPER_DEADLINE,
        Vec::new(),
        CredentialFailure::Fail,
    )
    .unwrap();
    let provider = config
        .provider("https://secret-origin.example", CredentialScope::Read)
        .unwrap();

    let error = helper
        .run_after_start(provider.credential())
        .await
        .unwrap_err()
        .to_string();

    assert_eq!(error, "credential helper exited unsuccessfully");
}

#[tokio::test]
async fn test_helper_spawn_failure_is_redacted() {
    let config = ExecCredentialConfig::new(
        vec!["/missing/secret-helper".to_owned()],
        Duration::from_secs(1),
        Vec::new(),
        CredentialFailure::Fail,
    )
    .unwrap();
    let provider = config
        .provider("https://secret-origin.example", CredentialScope::Read)
        .unwrap();

    let error = provider.credential().await.unwrap_err().to_string();

    assert_eq!(error, "credential helper failed to start");
}

#[tokio::test]
async fn test_helper_output_is_bounded() {
    let helper = Helper::script(|_, _| {
        "/bin/cat >/dev/null\ni=0\nwhile [ \"$i\" -lt 1025 ]; do\n  printf '%064d' 0\n  i=$((i + 1))\ndone\n".to_owned()
    });
    let provider = helper
        .config(CredentialFailure::Fail)
        .provider("https://resources.example", CredentialScope::Read)
        .unwrap();

    let error = helper.run_after_start(provider.credential()).await.unwrap_err();

    assert_eq!(error.to_string(), "credential helper output exceeded its limit");
}

#[tokio::test]
async fn test_helper_timeout_kills_descendants() {
    let helper = Helper::script(|_, requests| {
        format!(
            "/bin/cat >/dev/null\nexec 3> {}\n/usr/bin/tail -f /dev/null &\nprintf '%s %s' \"$$\" \"$!\" >&3\nexec /usr/bin/tail -f /dev/null\n",
            shell_quote(requests.display())
        )
    });
    let mut signal = helper.process_signal();
    let config = ExecCredentialConfig::new(
        vec![helper.executable.display().to_string()],
        Duration::from_mins(1),
        Vec::new(),
        CredentialFailure::Fail,
    )
    .unwrap();
    let provider = config
        .provider("https://resources.example", CredentialScope::Read)
        .unwrap();
    let credential = tokio::spawn(async move { provider.credential().await });
    let processes = signal.read().await;
    signal.release_guard();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(61)).await;

    let error = credential.await.unwrap().unwrap_err();
    let descendant = processes.split_once(' ').unwrap().1;
    let state = Command::new("ps")
        .args(["-o", "state=", "-p", descendant.trim()])
        .output()
        .unwrap()
        .stdout;

    assert_eq!(error.to_string(), "credential helper timed out");
    assert!(state.is_empty() || state.starts_with(b"Z"));
}

#[tokio::test]
async fn test_helper_cancellation_kills_descendants() {
    let helper = Helper::script(|_, requests| {
        format!(
            "/bin/cat >/dev/null\nexec 3> {}\n/usr/bin/tail -f /dev/null &\nprintf '%s %s' \"$$\" \"$!\" >&3\nexec /usr/bin/tail -f /dev/null\n",
            shell_quote(requests.display())
        )
    });
    let mut signal = helper.process_signal();
    let provider = helper
        .config(CredentialFailure::Fail)
        .provider("https://resources.example", CredentialScope::Read)
        .unwrap();
    let credential = tokio::spawn(async move { provider.credential().await });
    let processes = signal.read().await;
    signal.release_guard();
    let descendant = processes.split_once(' ').unwrap().1;
    let mut child_exits = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child()).unwrap();

    credential.abort();
    assert!(credential.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(5), child_exits.recv())
        .await
        .unwrap()
        .unwrap();
    let state = Command::new("ps")
        .args(["-o", "state=", "-p", descendant.trim()])
        .output()
        .unwrap()
        .stdout;

    assert!(state.is_empty() || state.starts_with(b"Z"));
}

#[tokio::test]
async fn test_helper_timeout_after_closing_stdout() {
    let response = bearer_response("unused");
    let helper = Helper::script(|_, marker| {
        format!(
            "/bin/cat >/dev/null\nprintf '%s' {}\nexec 1>&-\nread signal < {}\nexec /usr/bin/tail -f /dev/null\n",
            shell_quote(response),
            shell_quote(marker.display())
        )
    });
    let requests = helper.requests_fifo();
    let config = ExecCredentialConfig::new(
        vec![helper.executable.display().to_string()],
        Duration::from_mins(1),
        Vec::new(),
        CredentialFailure::Fail,
    )
    .unwrap();
    let provider = config
        .provider("https://resources.example", CredentialScope::Read)
        .unwrap();
    let credential = tokio::spawn(async move { provider.credential().await });
    helper.await_start().await;
    Helper::write(&requests, b"ready\n").await;
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(61)).await;

    let error = credential.await.unwrap().unwrap_err();

    assert_eq!(error.to_string(), "credential helper timed out");
}

#[tokio::test]
async fn test_helper_can_close_stdout_before_exiting() {
    let response = bearer_response("token");
    let helper = Helper::script(|_, signal| {
        format!(
            "/bin/cat >/dev/null\nprintf '%s' {}\nexec 1>&-\nread signal < {}\n",
            shell_quote(response),
            shell_quote(signal.display())
        )
    });
    let requests = helper.requests_fifo();
    let provider = helper
        .config(CredentialFailure::Fail)
        .provider("https://resources.example", CredentialScope::Read)
        .unwrap();
    let credential = tokio::spawn(async move { provider.credential().await });

    helper.await_start().await;
    Helper::write(&requests, b"ready\n").await;

    let snapshot = credential.await.unwrap().unwrap();

    assert_eq!(snapshot.auth(), &Auth::Bearer("token".to_owned()));
}

#[tokio::test]
async fn test_helper_can_exit_before_a_descendant_finishes_stdout() {
    let response = bearer_response("token");
    let helper = Helper::script(|_, signal| {
        format!(
            "/bin/cat >/dev/null\n/usr/bin/mkfifo {0}\n(/bin/cat < {0} >/dev/null; printf '%s' {1}) &\nexec 3> {0}\nexit 0\n",
            shell_quote(signal.display()),
            shell_quote(response)
        )
    });
    let provider = helper
        .config(CredentialFailure::Fail)
        .provider("https://resources.example", CredentialScope::Read)
        .unwrap();

    let snapshot = helper.run_after_start(provider.credential()).await.unwrap();

    assert_eq!(snapshot.auth(), &Auth::Bearer("token".to_owned()));
}

#[rstest]
#[case::cleared(Vec::new(), "absent")]
#[case::allowlisted(vec!["CARGO_MANIFEST_DIR".to_owned()], "present")]
#[tokio::test]
async fn test_helper_environment_is_cleared_then_allowlisted(#[case] environment: Vec<String>, #[case] expected: &str) {
    let helper = Helper::script(|_, _| {
        format!(
            "request=$(/bin/cat)\nif [ \"${{CARGO_MANIFEST_DIR+x}}\" = x ]; then\n  printf '%s' {}\nelse\n  printf '%s' {}\nfi\n",
            shell_quote(bearer_response("present")),
            shell_quote(bearer_response("absent")),
        )
    });
    let config = ExecCredentialConfig::new(
        vec![helper.executable.display().to_string()],
        NORMAL_HELPER_DEADLINE,
        environment,
        CredentialFailure::Fail,
    )
    .unwrap();
    let provider = config
        .provider("https://resources.example", CredentialScope::Read)
        .unwrap();

    let snapshot = helper.run_after_start(provider.credential()).await.unwrap();

    assert_eq!(snapshot.auth(), &Auth::Bearer(expected.to_owned()));
}

#[tokio::test]
async fn test_concurrent_requests_execute_one_helper() {
    let response = bearer_response("shared");
    let helper = Helper::script(|executions, signal| {
        format!(
            "/bin/cat >/dev/null\nprintf x >> {}\nread signal < {}\nprintf '%s' {}\n",
            shell_quote(executions.display()),
            shell_quote(signal.display()),
            shell_quote(response),
        )
    });
    let requests = helper.requests_fifo();
    let provider = helper
        .config(CredentialFailure::Fail)
        .provider("https://resources.example", CredentialScope::Read)
        .unwrap();

    let (snapshots, ()) = tokio::join!(join_all((0..50).map(|_| provider.credential())), async {
        helper.await_start().await;
        Helper::write(&requests, b"ready\n").await;
    });

    assert_eq!(helper.execution_count(), 1);
    assert!(
        snapshots
            .into_iter()
            .all(|snapshot| { snapshot.is_ok_and(|snapshot| snapshot.auth() == &Auth::Bearer("shared".to_owned())) })
    );
}

#[tokio::test]
async fn test_unauthorized_generation_executes_one_refresh() {
    let helper = rotating_helper();
    let provider = helper
        .config(CredentialFailure::Fail)
        .provider("https://resources.example", CredentialScope::Read)
        .unwrap();
    let first = helper.run_after_start(provider.credential()).await.unwrap();

    let snapshots = helper
        .run_after_start(join_all(
            (0..50).map(|_| provider.refresh_after_unauthorized(first.generation())),
        ))
        .await;

    assert_eq!(helper.execution_count(), 2);
    assert!(
        snapshots
            .into_iter()
            .all(|snapshot| { snapshot.is_ok_and(|snapshot| snapshot.auth() == &Auth::Bearer("new".to_owned())) })
    );
}

#[tokio::test]
async fn test_client_replays_one_unauthorized_exec_credential() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/resource/"))
        .and(header("authorization", "Bearer old"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/resource/"))
        .and(header("authorization", "Bearer new"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let helper = rotating_helper();
    let upstream = format!("{}/api/", server.uri());
    let client = UpstreamClient::with_credentials_and_tls_for_origin(
        &upstream,
        helper
            .config(CredentialFailure::Fail)
            .provider(&upstream, CredentialScope::Read)
            .unwrap(),
        &UpstreamTls::default(),
        &upstream,
        &[],
    )
    .unwrap();

    let response = helper
        .run_after_start(client.send_conditional(client.base().join("resource/").unwrap(), "application/json", None))
        .await
        .unwrap();

    assert_eq!(
        (response.status(), helper.execution_count()),
        (reqwest::StatusCode::OK, 2)
    );
}
