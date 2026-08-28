#![cfg(any(unix, feature = "self-update"))]

use std::{
    ffi::OsStr,
    path::Path,
    process::{Command, Output},
};

#[cfg(unix)]
use std::{
    ffi::OsString,
    io::{BufRead as _, Read as _, Write as _},
    net::{SocketAddr, TcpListener, TcpStream},
    os::fd::OwnedFd,
    os::unix::ffi::OsStringExt as _,
    os::unix::process::ExitStatusExt as _,
    process::{Child, ExitStatus, Stdio},
    sync::mpsc,
    time::Duration,
};

#[cfg(unix)]
const PUBLIC_LISTENER_FD_ENV: &str = "PERYX_INHERITED_PUBLIC_LISTENER_FD";

#[cfg(unix)]
const UPLOAD_BODY: &[u8] =
    b"--peryx\r\nContent-Disposition: form-data; name=\":action\"\r\n\r\nfile_upload\r\n--peryx--\r\n";

#[cfg(unix)]
const SHUTDOWN_CONFIG: &str = r#"
writer_identity = "writer"

[log]
level = "info"
format = "json"
sink = "stdout"

[availability]
mode = "dc"
group = "shutdown-test"

[availability.replication]
role = "primary"
source = "writer"
token = "replication-token"

[[availability.member]]
node = "writer"
dc = "dc-a"
address = "http://127.0.0.1:9001"
role = "writer"

[[availability.member]]
node = "replica"
dc = "dc-a"
address = "http://127.0.0.1:9002"
role = "replica"

[[index]]
ecosystem = "pypi"
name = "shutdown-hosted"
hosted = true

[[index.access_token]]
name = "upload"
secret = "secret"
actions = ["write"]

[[index.webhook]]
name = "audit"
url = "http://127.0.0.1:1/events"
secret = "secret"
events = ["upload"]
"#;

#[cfg(unix)]
#[test]
fn inherited_descriptors_are_adopted() {
    for descriptor in [3, 9] {
        assert_descriptor_adoption(descriptor);
    }
}

#[cfg(unix)]
#[test]
fn inherited_descriptor_errors_are_specific() {
    for (case, descriptor, expected) in [
        (
            "invalid-utf8",
            OsString::from_vec(vec![0xff]),
            format!("{PUBLIC_LISTENER_FD_ENV} is not valid UTF-8"),
        ),
        (
            "not-a-number",
            OsString::from("invalid"),
            format!("parse listener descriptor from {PUBLIC_LISTENER_FD_ENV}"),
        ),
        (
            "closed",
            OsString::from(i32::MAX.to_string()),
            format!("duplicate listener descriptor from {PUBLIC_LISTENER_FD_ENV}"),
        ),
        (
            "not-a-socket",
            OsString::from("0"),
            format!("inspect listener descriptor from {PUBLIC_LISTENER_FD_ENV}"),
        ),
    ] {
        let data = tempfile::tempdir().expect("create data directory");
        let output = serve_command(data.path(), "127.0.0.1:1".parse().expect("fixed address"))
            .env(PUBLIC_LISTENER_FD_ENV, descriptor)
            .stdin(Stdio::null())
            .output()
            .expect("run fixture");

        assert!(!output.status.success(), "descriptor case {case} succeeded");
        let error = stderr(&output);
        assert!(error.contains(&expected), "descriptor case {case}: {error}");
    }
}

#[cfg(unix)]
#[rstest::rstest]
#[case::sigterm(nix::sys::signal::Signal::SIGTERM)]
#[case::sigint(nix::sys::signal::Signal::SIGINT)]
fn shutdown_signal_drains_an_in_flight_upload(#[case] signal: nix::sys::signal::Signal) {
    let process = RunningServe::start();
    let upload = process.begin_upload();

    process.signal(signal);
    process.wait_for_shutdown();
    let response = finish_upload(upload);
    let (status, logs) = process.wait();

    assert!(
        response.starts_with("HTTP/1.1 400"),
        "unexpected upload response: {response}"
    );
    assert!(status.success(), "serve exited on signal {:?}: {logs}", status.signal());
    assert_eq!(logs.matches("shutdown signal received").count(), 1, "{logs}");
    for resource in ["webhook delivery", "local scheduler", "availability"] {
        assert!(
            has_shutdown_result(&logs, resource),
            "missing {resource} shutdown result: {logs}"
        );
    }
}

#[cfg(unix)]
#[test]
fn a_second_shutdown_signal_uses_the_default_disposition() {
    let process = RunningServe::start();
    let _upload = process.begin_upload();

    process.signal(nix::sys::signal::Signal::SIGTERM);
    process.wait_for_shutdown();
    process.signal(nix::sys::signal::Signal::SIGTERM);
    let (status, logs) = process.wait();

    assert_eq!(status.signal(), Some(nix::libc::SIGTERM), "{logs}");
    assert_eq!(logs.matches("shutdown signal received").count(), 1, "{logs}");
}

#[cfg(feature = "self-update")]
#[test]
fn self_update_uses_an_install_receipt() {
    let dir = tempfile::tempdir().expect("create receipt directory");
    let config_dir = dir.path().join("peryx");
    std::fs::create_dir(&config_dir).expect("create config directory");
    std::fs::write(
        config_dir.join("peryx-receipt.json"),
        format!(
            r#"{{
                "install_prefix": {:?},
                "binaries": ["peryx"],
                "source": {{"release_type": "github", "owner": "tox-dev", "name": "peryx", "app_name": "peryx"}},
                "version": "0.0.0",
                "provider": {{"source": "cargo-dist", "version": "0.23.0"}},
                "modify_path": false
            }}"#,
            dir.path().join("unrelated-install").display().to_string()
        ),
    )
    .expect("write install receipt");

    assert!(self_update_output(dir.path()).status.success());
    let missing = tempfile::tempdir().expect("create empty config directory");
    let output = self_update_output(missing.path());
    assert!(!output.status.success());
    let error = stderr(&output);
    assert!(error.contains("no install receipt found"), "{error}");
}

#[cfg(unix)]
fn assert_descriptor_adoption(descriptor: u8) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("reserve inherited listener");
    let address = listener.local_addr().expect("read inherited listener address");
    let data = tempfile::tempdir().expect("create data directory");
    let stderr = data.path().join("stderr.log");
    let mut command = Command::new("sh");
    command
        .args(["-c", &format!("exec {descriptor}<&0 0</dev/null; exec \"$0\" \"$@\"")])
        .arg(peryx_test_support::cargo_binary("peryx-process-fixture"))
        .arg("serve")
        .args([
            "--host",
            &address.ip().to_string(),
            "--port",
            &address.port().to_string(),
        ])
        .arg("--data-dir")
        .arg(data.path())
        .env(PUBLIC_LISTENER_FD_ENV, descriptor.to_string())
        .stdin(Stdio::from(OwnedFd::from(listener)))
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&stderr).expect("create fixture stderr"));
    let _child = ChildGuard(command.spawn().expect("start fixture"));
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("build HTTP client");

    let response = client
        .get(format!("http://{address}/+health"))
        .send()
        .expect("request inherited listener");

    assert_eq!(response.status(), reqwest::StatusCode::OK);
}

#[cfg(unix)]
fn serve_command(data: &Path, address: SocketAddr) -> Command {
    let mut command = fixture_command("serve");
    command
        .args([
            "--host",
            &address.ip().to_string(),
            "--port",
            &address.port().to_string(),
        ])
        .arg("--data-dir")
        .arg(data);
    command
}

#[cfg(unix)]
struct RunningServe {
    address: SocketAddr,
    child: ChildGuard,
    shutdown: mpsc::Receiver<()>,
    logs: std::thread::JoinHandle<String>,
    _data: tempfile::TempDir,
}

#[cfg(unix)]
impl RunningServe {
    fn start() -> Self {
        let data = tempfile::tempdir().expect("create data directory");
        let config = data.path().join("peryx.toml");
        std::fs::write(&config, SHUTDOWN_CONFIG).expect("write fixture config");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture listener");
        let address = listener.local_addr().expect("read fixture listener address");
        let mut command = serve_command(data.path(), address);
        command
            .arg("--config")
            .arg(config)
            .env(PUBLIC_LISTENER_FD_ENV, "0")
            .stdin(Stdio::from(OwnedFd::from(listener)))
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = ChildGuard(command.spawn().expect("start fixture"));
        let stdout = child.0.stdout.take().expect("capture fixture stdout");
        let (listening, started) = mpsc::sync_channel(0);
        let (shutdown, stopped) = mpsc::sync_channel(0);
        let logs = std::thread::spawn(move || {
            let mut output = String::new();
            for line in std::io::BufReader::new(stdout).lines() {
                let line = line.expect("read fixture log");
                let entry = serde_json::from_str::<serde_json::Value>(&line).expect("parse fixture JSON log");
                match entry.pointer("/fields/message").and_then(serde_json::Value::as_str) {
                    Some("peryx listening") => {
                        let _ = listening.send(());
                    }
                    Some("shutdown signal received") => {
                        let _ = shutdown.send(());
                    }
                    _ => {}
                }
                output.push_str(&line);
                output.push('\n');
            }
            output
        });
        started.recv().expect("fixture reports readiness");
        Self {
            address,
            child,
            shutdown: stopped,
            logs,
            _data: data,
        }
    }

    fn begin_upload(&self) -> std::io::BufReader<TcpStream> {
        let mut stream = std::io::BufReader::new(TcpStream::connect(self.address).expect("connect to fixture"));
        write!(
            stream.get_mut(),
            "POST /shutdown-hosted/ HTTP/1.1\r\nHost: {}\r\nAuthorization: Basic X190b2tlbl9fOnNlY3JldA==\r\nContent-Type: multipart/form-data; boundary=peryx\r\nContent-Length: {}\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n",
            self.address,
            UPLOAD_BODY.len(),
        )
        .expect("write upload headers");
        assert_eq!(read_response_status(&mut stream), "HTTP/1.1 100 Continue\r\n");
        stream
            .get_mut()
            .write_all(&UPLOAD_BODY[..UPLOAD_BODY.len() / 2])
            .expect("write upload prefix");
        stream
    }

    fn signal(&self, signal: nix::sys::signal::Signal) {
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(i32::try_from(self.child.0.id()).expect("child PID fits i32")),
            signal,
        )
        .expect("signal fixture");
    }

    fn wait_for_shutdown(&self) {
        self.shutdown.recv().expect("fixture reports shutdown signal");
    }

    fn wait(self) -> (ExitStatus, String) {
        let Self {
            mut child, logs, _data, ..
        } = self;
        let status = child.0.wait().expect("wait for fixture");
        (status, logs.join().expect("join log reader"))
    }
}

#[cfg(unix)]
fn finish_upload(mut stream: std::io::BufReader<TcpStream>) -> String {
    stream
        .get_mut()
        .write_all(&UPLOAD_BODY[UPLOAD_BODY.len() / 2..])
        .expect("finish upload body");
    let status = read_response_status(&mut stream);
    let mut body = Vec::new();
    stream.read_to_end(&mut body).expect("read upload response");
    status
}

#[cfg(unix)]
fn read_response_status(stream: &mut std::io::BufReader<TcpStream>) -> String {
    let mut status = String::new();
    stream.read_line(&mut status).expect("read response status");
    let mut header = String::new();
    while stream.read_line(&mut header).expect("read response header") != 0 && header != "\r\n" {
        header.clear();
    }
    status
}

#[cfg(unix)]
fn has_shutdown_result(logs: &str, resource: &str) -> bool {
    logs.lines().any(|line| {
        serde_json::from_str::<serde_json::Value>(line).is_ok_and(|entry| {
            entry.pointer("/fields/message").and_then(serde_json::Value::as_str) == Some("resource shutdown completed")
                && entry.pointer("/fields/resource").and_then(serde_json::Value::as_str) == Some(resource)
        })
    })
}

#[cfg(feature = "self-update")]
fn self_update_output(config_home: &Path) -> Output {
    fixture_command("self-update")
        .env("XDG_CONFIG_HOME", config_home)
        .output()
        .expect("run self-update fixture")
}

fn fixture_command(command: impl AsRef<OsStr>) -> Command {
    let mut fixture = Command::new(peryx_test_support::cargo_binary("peryx-process-fixture"));
    fixture.arg(command);
    fixture
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[cfg(unix)]
struct ChildGuard(Child);

#[cfg(unix)]
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
