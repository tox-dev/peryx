use std::io::{Cursor, Read};
use std::path::PathBuf;

use peryx_driver::users::UserService;
use peryx_storage::meta::MetaStore;
use rstest::rstest;

use crate::app::bootstrap_administrator;
use crate::cli::{BootstrapAdministratorArgs, RuntimeArgs};
use crate::config::Config;

#[test]
fn test_bootstrap_administrator_reads_stdin_without_disclosing_the_password() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir);
    let password = "correct horse battery staple";
    let mut input = Cursor::new(format!("{password}\r\n"));
    let mut output = Vec::new();

    bootstrap_administrator(&config, &stdin_args(), &mut input, &mut output).unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(output.starts_with("administrator\tusr_"));
    assert!(output.ends_with("\tAlice\n"));
    assert!(!output.contains(password));
    assert!(authenticate(&config, "alice", password).is_some());
}

#[test]
fn test_bootstrap_administrator_reads_a_secret_file_and_strips_one_lf() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir);
    let path = dir.path().join("administrator-password");
    let password = "  pāssword with whitespace  \n";
    std::fs::write(&path, format!("{password}\n")).unwrap();

    bootstrap_administrator(&config, &file_args(path), &mut Cursor::new(Vec::new()), &mut Vec::new()).unwrap();

    assert!(authenticate(&config, "Alice", password).is_some());
}

#[test]
fn test_bootstrap_administrator_refuses_a_second_attempt_without_secret_output() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir);
    let password = "correct horse battery staple";
    bootstrap_administrator(&config, &stdin_args(), &mut Cursor::new(password), &mut Vec::new()).unwrap();
    let second_password = "another administrator password";
    let mut output = Vec::new();

    let error = bootstrap_administrator(
        &config,
        &BootstrapAdministratorArgs {
            display_name: "Bob".to_owned(),
            ..stdin_args()
        },
        &mut Cursor::new(second_password),
        &mut output,
    )
    .unwrap_err();

    assert!(error.to_string().contains("administrator grant already exists"));
    assert!(!error.to_string().contains(second_password));
    assert!(output.is_empty());
    assert_eq!(
        MetaStore::open_existing(config.data_dir.join("peryx.redb"))
            .unwrap()
            .get_user_by_name("Bob")
            .unwrap(),
        None
    );
}

#[rstest]
#[case::short(b"short\n".to_vec(), "at least 15 characters")]
#[case::too_many_characters(vec![b'a'; 1_025], "at most 1024 characters")]
#[case::not_utf8(vec![0xff; 16], "password input must be UTF-8")]
#[case::too_many_bytes(vec![b'a'; 1_048_577], "exceeds the 1048576-byte limit")]
fn test_bootstrap_administrator_rejects_invalid_password_input(#[case] input: Vec<u8>, #[case] expected: &str) {
    let dir = tempfile::tempdir().unwrap();
    let error =
        bootstrap_administrator(&config(&dir), &stdin_args(), &mut Cursor::new(input), &mut Vec::new()).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains(expected), "{message}");
}

#[rstest]
#[case::minimum("a".repeat(15))]
#[case::maximum("🦀".repeat(1_024))]
fn test_bootstrap_administrator_accepts_password_length_boundaries(#[case] password: String) {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir);

    bootstrap_administrator(
        &config,
        &stdin_args(),
        &mut Cursor::new(password.as_bytes()),
        &mut Vec::new(),
    )
    .unwrap();

    assert!(authenticate(&config, "Alice", &password).is_some());
}

#[test]
fn test_bootstrap_administrator_rejects_read_only_mode() {
    let dir = tempfile::tempdir().unwrap();
    let mut read_only = config(&dir);
    read_only.read_only = true;
    assert_eq!(
        bootstrap_administrator(
            &read_only,
            &stdin_args(),
            &mut Cursor::new("correct horse battery staple"),
            &mut Vec::new()
        )
        .unwrap_err()
        .to_string(),
        "cannot bootstrap an administrator in read-only mode"
    );
}

#[test]
fn test_bootstrap_administrator_reports_a_stdin_read_failure() {
    let dir = tempfile::tempdir().unwrap();
    let error = bootstrap_administrator(&config(&dir), &stdin_args(), &mut FailRead, &mut Vec::new()).unwrap_err();
    assert!(error.to_string().contains("read password from standard input"));
}

#[test]
fn test_bootstrap_administrator_reports_a_missing_secret_file() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing-secret");
    let error = bootstrap_administrator(
        &config(&dir),
        &file_args(missing.clone()),
        &mut Cursor::new(Vec::new()),
        &mut Vec::new(),
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains(&format!("open password file {}", missing.display()))
    );
}

#[test]
fn test_bootstrap_administrator_reports_data_directory_initialization_failure() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("file");
    std::fs::write(&parent, b"not a directory").unwrap();
    let config = Config {
        data_dir: parent.join("data"),
        ..Config::default()
    };

    let error = bootstrap_administrator(
        &config,
        &stdin_args(),
        &mut Cursor::new("correct horse battery staple"),
        &mut Vec::new(),
    )
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains(&format!("initialize data directory {}", config.data_dir.display()))
    );
}

#[test]
fn test_bootstrap_administrator_reports_metadata_open_failure() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir);
    std::fs::create_dir_all(config.data_dir.join("peryx.redb")).unwrap();

    let error = bootstrap_administrator(
        &config,
        &stdin_args(),
        &mut Cursor::new("correct horse battery staple"),
        &mut Vec::new(),
    )
    .unwrap_err();

    assert!(error.to_string().contains(&format!(
        "open metadata store {}",
        config.data_dir.join("peryx.redb").display()
    )));
}

fn config(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().join("data"),
        ..Config::default()
    }
}

fn stdin_args() -> BootstrapAdministratorArgs {
    BootstrapAdministratorArgs {
        runtime: RuntimeArgs::default(),
        display_name: "Alice".to_owned(),
        password_stdin: true,
        password_file: None,
    }
}

fn authenticate(config: &Config, display_name: &str, password: &str) -> Option<peryx_identity::UserId> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(
            UserService::new(MetaStore::open_existing(config.data_dir.join("peryx.redb")).unwrap())
                .authenticate(display_name, password),
        )
        .unwrap()
}

fn file_args(path: PathBuf) -> BootstrapAdministratorArgs {
    BootstrapAdministratorArgs {
        runtime: RuntimeArgs::default(),
        display_name: "Alice".to_owned(),
        password_stdin: false,
        password_file: Some(path),
    }
}

struct FailRead;

impl Read for FailRead {
    fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("read failed"))
    }
}
