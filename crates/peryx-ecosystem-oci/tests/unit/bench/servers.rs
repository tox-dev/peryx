use super::*;

#[cfg(unix)]
fn write_executable(path: &Path, body: impl AsRef<[u8]>) {
    use std::os::unix::fs::PermissionsExt as _;

    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
#[test]
fn crane_login_reaps_the_child_after_a_broken_stdin() {
    let directory = tempfile::tempdir().unwrap();
    let pid = directory.path().join("pid");
    let crane = directory.path().join("crane");
    write_executable(
        &crane,
        format!(
            "#!/bin/sh\nprintf '%s' $$ > '{}'\nexec 0<&-\nwhile :; do :; done\n",
            pid.display()
        ),
    );

    assert!(
        login_crane(&BenchEnvironment::new(
            Some(directory.path()),
            Some(("user".to_owned(), "x".repeat(1 << 20)))
        ))
        .is_err()
    );
    assert!(
        !std::process::Command::new("kill")
            .args(["-0", &std::fs::read_to_string(pid).unwrap()])
            .status()
            .unwrap()
            .success()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn readiness_pull_obeys_the_startup_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let gate = directory.path().join("gate");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&gate)
            .status()
            .unwrap()
            .success()
    );
    let crane = directory.path().join("crane");
    write_executable(&crane, format!("#!/bin/sh\ncat '{}'\n", gate.display()));
    let mut environment = BenchEnvironment::new(Some(directory.path()), None);
    environment.startup_timeout = std::time::Duration::from_millis(1);

    assert!(
        readiness_pull(
            &environment,
            "http://registry.test",
            READINESS_IMAGE,
            &directory.path().join("image.tar")
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("timed out")
    );
}

#[test]
fn capture_stream_records_lines_and_signals_the_marker() {
    let directory = tempfile::tempdir().unwrap();
    let log_path = directory.path().join("server.log");
    let log = Arc::new(Mutex::new(std::fs::File::create(&log_path).unwrap()));
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);

    capture_stream(
        std::io::Cursor::new(b"starting\nregistry ready\n"),
        log,
        sender,
        "ready",
    )
    .join()
    .unwrap();

    receiver.try_recv().unwrap();
    assert_eq!(std::fs::read_to_string(log_path).unwrap(), "starting\nregistry ready\n");
}

#[tokio::test]
async fn wait_for_startup_rejects_closed_output() {
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    drop(sender);

    assert_eq!(
        wait_for_startup(
            receiver,
            tokio::time::Instant::now() + std::time::Duration::from_secs(1)
        )
        .await
        .unwrap(),
        None
    );
}

#[cfg(unix)]
#[tokio::test]
async fn wait_for_container_event_runs_docker_logs() {
    let directory = tempfile::tempdir().unwrap();
    let arguments = directory.path().join("docker.args");
    write_executable(
        &directory.path().join("docker"),
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > '{}'\nprintf 'registry ready\\n'\n",
            arguments.display()
        ),
    );
    let environment = BenchEnvironment::new(Some(directory.path()), None);

    wait_for_container_event(
        &environment,
        "mirror",
        "ready",
        tokio::time::Instant::now() + environment.startup_timeout,
    )
    .await
    .unwrap();

    assert_eq!(std::fs::read_to_string(arguments).unwrap(), "logs --follow mirror\n");
}

#[cfg(unix)]
#[tokio::test]
async fn readiness_pull_runs_crane() {
    let directory = tempfile::tempdir().unwrap();
    let arguments = directory.path().join("crane.args");
    write_executable(
        &directory.path().join("crane"),
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > '{}'\nprintf image > \"$4\"\n",
            arguments.display()
        ),
    );
    let environment = BenchEnvironment::new(Some(directory.path()), None);
    let image = directory.path().join("image.tar");

    readiness_pull(&environment, "http://registry.test", "repo:tag", &image)
        .await
        .unwrap();

    assert_eq!(std::fs::read_to_string(&image).unwrap(), "image");
    assert_eq!(
        std::fs::read_to_string(arguments).unwrap(),
        format!("pull --insecure registry.test/repo:tag {}\n", image.display())
    );
}
