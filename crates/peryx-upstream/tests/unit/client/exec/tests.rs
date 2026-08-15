use std::process::Stdio;
use std::time::Duration;

use command_group::AsyncCommandGroup as _;
use tokio::io::AsyncReadExt as _;
use tokio::process::Command;

use super::{CredentialFailure, ExecCredentialConfig, MAX_OUTPUT_BYTES, ProcessGroup, reap, terminate};

fn spawn(script: &str) -> command_group::AsyncGroupChild {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.group().spawn().expect("the test shell is available")
}

#[tokio::test]
async fn test_terminate_returns_promptly_when_the_child_already_exited() {
    let mut child = spawn("exit 0");
    assert!(child.wait().await.expect("child is reaped").success());

    tokio::time::timeout(Duration::from_secs(5), terminate(&mut child))
        .await
        .expect("terminate returns without waiting on an already-exited child");

    assert_eq!(
        child
            .try_wait()
            .expect("status stays available")
            .map(|status| status.success()),
        Some(true)
    );
}

#[tokio::test]
async fn test_execute_rejects_output_over_the_limit() {
    let script = format!("head -c {} /dev/zero; exec tail -f /dev/null", MAX_OUTPUT_BYTES + 1);
    let config = ExecCredentialConfig::new(
        vec!["/bin/sh".to_owned(), "-c".to_owned(), script],
        Duration::from_secs(10),
        vec!["PATH".to_owned()],
        CredentialFailure::Fail,
    )
    .expect("the helper config is valid");

    let error = config
        .execute(b"request")
        .await
        .expect_err("output past the limit is rejected");

    assert!(error.to_string().contains("exceeded its limit"), "{error}");
}

#[tokio::test]
async fn test_terminate_kills_a_running_child() {
    let mut child = spawn("exec tail -f /dev/null");
    assert!(child.try_wait().expect("child is still running").is_none());

    tokio::time::timeout(Duration::from_secs(5), terminate(&mut child))
        .await
        .expect("terminate kills and reaps the running child");

    assert!(
        matches!(child.try_wait().expect("status is available after terminate"), Some(status) if !status.success())
    );
}

#[tokio::test]
async fn test_dropping_a_process_group_reaps_its_running_child() {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "exec tail -f /dev/null"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.group().spawn().expect("the test shell is available");
    let mut output = child.inner().stdout.take().expect("stdout is piped");
    drop(ProcessGroup {
        child: Some(child),
        direct_reaped: false,
    });

    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), output.read_to_end(&mut bytes))
        .await
        .expect("dropped process group closes stdout")
        .expect("read child stdout");
    assert!(bytes.is_empty());
}

#[tokio::test]
async fn test_process_reaper_waits_for_child() {
    tokio::time::timeout(Duration::from_secs(5), reap(spawn("exit 0")))
        .await
        .expect("the reaper completes")
        .expect("the reaper task does not panic");
}
