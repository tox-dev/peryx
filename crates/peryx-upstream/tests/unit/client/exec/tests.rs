use std::process::Stdio;
use std::time::Duration;

use command_group::AsyncCommandGroup as _;
use tokio::process::Command;

use super::{CredentialFailure, ExecCredentialConfig, MAX_OUTPUT_BYTES, terminate};

fn spawn(script: &str) -> command_group::AsyncGroupChild {
    Command::new("/bin/sh")
        .args(["-c", script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .group()
        .spawn()
        .expect("test helper child spawns")
}

// Reaping the child before terminate runs pins try_wait to the cached status, so the early
// return is taken on every run rather than only when the helper happens to have exited in time.
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

// The helper writes one byte past the output limit and then stays alive, so the reader fills and
// finishes while the child is still running. That drives the loop's over-limit branch deterministically
// on every run, rather than only when the helper happens to have exited in time. PATH is inherited so
// the shell resolves its commands after `execute` clears the environment.
#[tokio::test]
async fn test_execute_rejects_output_over_the_limit() {
    let script = format!("head -c {} /dev/zero; sleep 30", MAX_OUTPUT_BYTES + 1);
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
    let mut child = spawn("sleep 300");
    assert!(child.try_wait().expect("child is still running").is_none());

    tokio::time::timeout(Duration::from_secs(5), terminate(&mut child))
        .await
        .expect("terminate kills and reaps the running child");

    assert!(
        matches!(child.try_wait().expect("status is available after terminate"), Some(status) if !status.success())
    );
}
