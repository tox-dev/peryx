#![cfg(any(unix, feature = "self-update"))]

use std::{
    ffi::OsStr,
    path::Path,
    process::{Command, Output},
};

#[cfg(unix)]
use std::{
    ffi::OsString,
    net::{SocketAddr, TcpListener},
    os::fd::OwnedFd,
    os::unix::ffi::OsStringExt as _,
    process::{Child, Stdio},
    time::Duration,
};

#[cfg(unix)]
const PUBLIC_LISTENER_FD_ENV: &str = "PERYX_INHERITED_PUBLIC_LISTENER_FD";

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
    let _ = rustls::crypto::ring::default_provider().install_default();
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
