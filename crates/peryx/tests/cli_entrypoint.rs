#![cfg(feature = "system-tests")]

use rstest::rstest;
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

struct Store {
    dir: tempfile::TempDir,
    data: PathBuf,
}

impl Store {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let output = run([OsStr::new("init"), OsStr::new("--data-dir"), data.as_os_str()]);
        assert_success(&output);
        let password = dir.path().join("password");
        std::fs::write(&password, "correct-horse-battery-staple").unwrap();
        let output = run([
            OsStr::new("bootstrap-administrator"),
            OsStr::new("Administrator"),
            OsStr::new("--password-file"),
            password.as_os_str(),
            OsStr::new("--data-dir"),
            data.as_os_str(),
        ]);
        assert_success(&output);
        Self { dir, data }
    }

    fn command(&self, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Command {
        let mut command = peryx();
        command
            .args(args)
            .args([OsStr::new("--data-dir"), self.data.as_os_str()]);
        command
    }

    fn dc_primary_config(&self) -> PathBuf {
        let config = self.dir.path().join("dc-primary.toml");
        std::fs::write(
            &config,
            r#"[availability]
mode = "dc"

[availability.replication]
role = "primary"
source = "writer"
token = "replication-token"

[availability.write_ack]
policy = "local"
"#,
        )
        .unwrap();
        config
    }
}

fn peryx() -> Command {
    Command::new(peryx_test_support::cargo_binary("peryx"))
}

fn run(args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Output {
    peryx().args(args).output().unwrap()
}

fn assert_success(output: &Output) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
}

fn assert_failure(output: &Output, expected: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains(expected), "{stderr}");
}

fn output_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(name)
}

#[test]
fn openapi_command_runs_the_binary_entrypoint() {
    let output = run(["openapi"]);

    assert_success(&output);
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["info"]["title"], "peryx");
}

#[test]
fn init_command_installs_file_logging_and_creates_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let data = output_path(dir.path(), "data");
    let log = output_path(dir.path(), "peryx.log");
    let output = run([
        OsStr::new("init"),
        OsStr::new("--data-dir"),
        data.as_os_str(),
        OsStr::new("--log-sink"),
        OsStr::new("file"),
        OsStr::new("--log-format"),
        OsStr::new("json"),
        OsStr::new("--log-file"),
        log.as_os_str(),
    ]);

    assert_success(&output);
    assert!(data.is_dir());
    assert!(log.parent().unwrap().is_dir());
}

#[test]
fn config_check_accepts_an_initialized_store() {
    let store = Store::new();
    let output = store.command(["config", "check"]).output().unwrap();

    assert_success(&output);
}

#[test]
fn index_list_reports_the_configured_indexes() {
    let store = Store::new();
    let output = store.command(["index", "list"]).output().unwrap();

    assert_success(&output);
    assert!(!output.stdout.is_empty());
}

#[test]
fn job_list_accepts_an_empty_store() {
    let store = Store::new();
    let output = store.command(["job", "list"]).output().unwrap();

    assert_success(&output);
}

#[test]
fn cache_size_reports_an_empty_store() {
    let store = Store::new();
    let output = store.command(["cache", "size"]).output().unwrap();

    assert_success(&output);
    assert!(!output.stdout.is_empty());
}

#[test]
fn policy_dry_run_accepts_an_empty_store() {
    let store = Store::new();
    let output = store.command(["policy", "dry-run"]).output().unwrap();

    assert_success(&output);
}

#[test]
fn quota_list_accepts_an_empty_store() {
    let store = Store::new();
    let output = store.command(["quota", "list"]).output().unwrap();

    assert_success(&output);
}

#[test]
fn config_snippet_reports_an_unreadable_config() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("missing.toml");
    let output = run([
        OsStr::new("config-snippet"),
        OsStr::new("client.conf"),
        OsStr::new("--config"),
        config.as_os_str(),
        OsStr::new("--base-url"),
        OsStr::new("http://localhost:8080"),
        OsStr::new("--index"),
        OsStr::new("repository"),
    ]);

    assert_failure(&output, "missing.toml");
}

#[test]
fn backup_create_and_verify_round_trip() {
    let store = Store::new();
    let dir = tempfile::tempdir().unwrap();
    let backup = output_path(dir.path(), "backup");
    let create = store
        .command([OsStr::new("backup"), OsStr::new("create"), backup.as_os_str()])
        .output()
        .unwrap();

    assert_success(&create);
    let verify = run([OsStr::new("backup"), OsStr::new("verify"), backup.as_os_str()]);
    assert_success(&verify);
}

#[test]
fn backup_create_publishes_into_a_target_the_caller_reserved() {
    let store = Store::new();
    let dir = tempfile::tempdir().unwrap();
    let backup = output_path(dir.path(), "backup");
    std::fs::create_dir(&backup).unwrap();

    let create = store
        .command([OsStr::new("backup"), OsStr::new("create"), backup.as_os_str()])
        .output()
        .unwrap();

    assert_success(&create);
    assert_success(&run([OsStr::new("backup"), OsStr::new("verify"), backup.as_os_str()]));
}

#[test]
fn backup_create_leaves_a_published_backup_intact() {
    let store = Store::new();
    let dir = tempfile::tempdir().unwrap();
    let backup = output_path(dir.path(), "backup");
    assert_success(
        &store
            .command([OsStr::new("backup"), OsStr::new("create"), backup.as_os_str()])
            .output()
            .unwrap(),
    );

    let second = store
        .command([OsStr::new("backup"), OsStr::new("create"), backup.as_os_str()])
        .output()
        .unwrap();

    assert_failure(&second, "is not empty");
    assert_success(&run([OsStr::new("backup"), OsStr::new("verify"), backup.as_os_str()]));
}

#[test]
fn restore_recovers_a_backup_into_an_empty_directory() {
    let store = Store::new();
    let dir = tempfile::tempdir().unwrap();
    let backup = output_path(dir.path(), "backup");
    assert_success(
        &store
            .command([OsStr::new("backup"), OsStr::new("create"), backup.as_os_str()])
            .output()
            .unwrap(),
    );
    let restored = output_path(dir.path(), "restored");
    let output = run([
        OsStr::new("restore"),
        backup.as_os_str(),
        OsStr::new("--data-dir"),
        restored.as_os_str(),
    ]);

    assert_success(&output);
    assert!(restored.is_dir());
}

#[test]
fn import_dir_reports_an_unknown_index() {
    let store = Store::new();
    let source = tempfile::tempdir().unwrap();
    let output = store
        .command([
            OsStr::new("import-dir"),
            OsStr::new("missing"),
            source.path().as_os_str(),
        ])
        .output()
        .unwrap();

    assert_failure(&output, "missing");
}

#[test]
fn retention_dry_run_reports_an_unknown_index() {
    let store = Store::new();
    let output = store
        .command(["retention", "dry-run", "--index", "missing"])
        .output()
        .unwrap();

    assert_failure(&output, "missing");
}

#[test]
fn writer_claim_succeeds_for_a_local_store() {
    let store = Store::new();
    let config = store.dc_primary_config();
    let output = store
        .command([
            OsStr::new("writer"),
            OsStr::new("claim"),
            OsStr::new("--writer-identity"),
            OsStr::new("writer-a"),
            OsStr::new("--config"),
            config.as_os_str(),
        ])
        .output()
        .unwrap();

    assert_success(&output);
}

#[test]
fn writer_promote_reaches_the_operator_contract() {
    let store = Store::new();
    let config = store.dc_primary_config();
    let output = store
        .command([
            OsStr::new("writer"),
            OsStr::new("promote"),
            OsStr::new("writer-b"),
            OsStr::new("--writer-identity"),
            OsStr::new("writer-a"),
            OsStr::new("--config"),
            config.as_os_str(),
        ])
        .output()
        .unwrap();

    assert_failure(&output, "writer");
}

#[test]
fn mirror_plan_reports_a_missing_repository() {
    let store = Store::new();
    let output = store.command(["mirror", "plan", "missing"]).output().unwrap();

    assert_failure(&output, "missing");
}

#[tokio::test]
async fn revocation_list_reports_an_unreachable_server() {
    let mut command = peryx();
    command.args([
        "revocation",
        "list",
        "--server",
        "http://127.0.0.1:1",
        "--user",
        "admin",
        "--password-stdin",
    ]);
    let mut child = tokio::process::Command::from(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"password\n").await.unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let stdout = tokio::spawn(async move {
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).await.unwrap();
        output
    });
    let mut stderr = child.stderr.take().unwrap();
    let stderr = tokio::spawn(async move {
        let mut output = Vec::new();
        stderr.read_to_end(&mut output).await.unwrap();
        output
    });
    let status = child.wait().await.unwrap();
    let output = Output {
        status,
        stdout: stdout.await.unwrap(),
        stderr: stderr.await.unwrap(),
    };

    assert_failure(&output, "127.0.0.1:1");
}

#[test]
fn bootstrap_admin_creates_an_account() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let password = dir.path().join("password");
    std::fs::write(&password, "correct-horse-battery-staple").unwrap();
    let output = run([
        OsStr::new("bootstrap-administrator"),
        OsStr::new("Administrator"),
        OsStr::new("--password-file"),
        password.as_os_str(),
        OsStr::new("--data-dir"),
        data.as_os_str(),
    ]);

    assert_success(&output);
    assert!(data.join("peryx.redb").is_file());
}

#[rstest]
#[case::serve(&["serve"])]
#[case::config_check(&["config", "check"])]
fn runtime_command_rejects_an_invalid_host(#[case] command: &[&str]) {
    let store = Store::new();
    let output = store
        .command(command.iter().copied())
        .args(["--host", "not a host"])
        .output()
        .unwrap();

    assert_failure(&output, "`host` \"not a host\"");
}

#[rstest]
#[case::serve_freshness(&["serve"], "PERYX_CACHE_TTL_SECS", "cache_ttl_secs")]
#[case::serve_stale_bound(&["serve"], "PERYX_MAX_STALE_SECS", "max_stale_secs")]
#[case::check_freshness(&["config", "check"], "PERYX_CACHE_TTL_SECS", "cache_ttl_secs")]
#[case::check_stale_bound(&["config", "check"], "PERYX_MAX_STALE_SECS", "max_stale_secs")]
fn runtime_commands_reject_negative_cache_timing(
    #[case] command: &[&str],
    #[case] variable: &str,
    #[case] field: &str,
) {
    let output = peryx().args(command).env(variable, "-1").output().unwrap();

    assert_failure(&output, &format!("`{field}` must be non-negative, got -1"));
}

#[test]
fn runtime_arguments_override_environment_and_selected_config() {
    let dir = tempfile::tempdir().unwrap();
    let config_data = dir.path().join("config-data");
    let environment_data = dir.path().join("environment-data");
    let argument_data = dir.path().join("argument-data");
    let config = dir.path().join("peryx.toml");
    std::fs::write(&config, format!("data_dir = {config_data:?}\n")).unwrap();

    let output = peryx()
        .args([
            OsStr::new("init"),
            OsStr::new("--config"),
            config.as_os_str(),
            OsStr::new("--data-dir"),
            argument_data.as_os_str(),
        ])
        .env("PERYX_DATA_DIR", &environment_data)
        .output()
        .unwrap();

    assert_success(&output);
    assert!(argument_data.is_dir());
    assert!(!config_data.exists());
    assert!(!environment_data.exists());
}

#[test]
fn distributed_startup_installs_services_before_listener_failure() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("peryx.toml");
    std::fs::write(
        &config,
        format!(
            r#"host = "invalid-address"
data_dir = {:?}
writer_identity = "writer"

[availability]
mode = "dc"

[availability.replication]
role = "primary"
source = "writer"
token = "replication-token"

[availability.write_ack]
policy = "local"
"#,
            dir.path().join("data")
        ),
    )
    .unwrap();

    let output = run([OsStr::new("serve"), OsStr::new("--config"), config.as_os_str()]);

    assert_failure(&output, "invalid-address");
}
