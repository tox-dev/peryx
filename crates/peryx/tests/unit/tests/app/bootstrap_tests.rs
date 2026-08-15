use std::io::{Cursor, Read};
use std::path::PathBuf;

use peryx_driver::users::UserService;
use peryx_storage::meta::MetaStore;
use rstest::rstest;

use super::*;
use crate::cli::RuntimeArgs;

#[rstest]
#[case::read_only(true, b"correct horse battery staple".to_vec(), "cannot bootstrap an administrator in read-only mode")]
#[case::short(false, b"short".to_vec(), "at least 15 characters")]
#[case::long(false, vec![b'a'; 1_025], "at most 1024 characters")]
#[case::not_utf8(false, vec![0xff; 16], "password input must be UTF-8")]
#[case::oversized(false, vec![b'a'; 1_048_577], "exceeds the 1048576-byte limit")]
fn test_bootstrap_administrator_rejects_invalid_mode_or_password(
    #[case] read_only: bool,
    #[case] password: Vec<u8>,
    #[case] expected: &str,
) {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().join("data"),
        read_only,
        ..Config::default()
    };

    let error = bootstrap_administrator(&config, &args(), &mut Cursor::new(password), &mut Vec::new()).unwrap_err();

    assert!(format!("{error:#}").contains(expected), "{error:#}");
    assert!(!config.data_dir.join("peryx.redb").exists());
}

#[test]
fn test_bootstrap_administrator_creates_the_first_user() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().join("data"),
        ..Config::default()
    };
    let mut output = Vec::new();

    bootstrap_administrator(
        &config,
        &args(),
        &mut Cursor::new(b"correct horse battery staple"),
        &mut output,
    )
    .unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(output.starts_with("administrator\t"), "{output}");
    assert!(output.ends_with("\tAlice\n"), "{output}");
    assert!(!output.contains("correct horse battery staple"), "{output}");
    assert!(config.data_dir.join("peryx.redb").is_file());
    assert!(authenticate(&config, "Alice", "correct horse battery staple").is_some());
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
fn test_bootstrap_administrator_rejects_a_second_grant_without_disclosing_input() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir);
    bootstrap_administrator(
        &config,
        &args(),
        &mut Cursor::new("correct horse battery staple"),
        &mut Vec::new(),
    )
    .unwrap();
    let password = "another administrator password";
    let mut output = Vec::new();

    let error = bootstrap_administrator(
        &config,
        &BootstrapAdministratorArgs {
            display_name: "Bob".to_owned(),
            ..args()
        },
        &mut Cursor::new(password),
        &mut output,
    )
    .unwrap_err();

    assert!(error.to_string().contains("administrator grant already exists"));
    assert!(!error.to_string().contains(password));
    assert!(output.is_empty());
    assert!(authenticate(&config, "Bob", password).is_none());
}

#[rstest]
#[case::minimum("a".repeat(15))]
#[case::maximum("🦀".repeat(1_024))]
fn test_bootstrap_administrator_accepts_password_length_boundaries(#[case] password: String) {
    let dir = tempfile::tempdir().unwrap();
    let config = config(&dir);

    bootstrap_administrator(&config, &args(), &mut Cursor::new(password.as_bytes()), &mut Vec::new()).unwrap();

    assert!(authenticate(&config, "Alice", &password).is_some());
}

#[test]
fn test_bootstrap_administrator_contextualizes_input_failures() {
    let dir = tempfile::tempdir().unwrap();
    let stdin_error = bootstrap_administrator(&config(&dir), &args(), &mut FailRead, &mut Vec::new()).unwrap_err();
    let missing = dir.path().join("missing-secret");
    let file_error = bootstrap_administrator(
        &config(&dir),
        &file_args(missing.clone()),
        &mut Cursor::new(Vec::new()),
        &mut Vec::new(),
    )
    .unwrap_err();

    assert!(stdin_error.to_string().contains("read password from standard input"));
    assert!(
        file_error
            .to_string()
            .contains(&format!("open password file {}", missing.display()))
    );
}

#[test]
fn test_bootstrap_administrator_contextualizes_filesystem_failures() {
    let init_dir = tempfile::tempdir().unwrap();
    let blocking_file = init_dir.path().join("file");
    std::fs::write(&blocking_file, b"block").unwrap();
    let init_config = Config {
        data_dir: blocking_file.join("data"),
        ..Config::default()
    };
    let init_error = format!(
        "{:#}",
        bootstrap_administrator(
            &init_config,
            &args(),
            &mut Cursor::new(b"correct horse battery staple"),
            &mut Vec::new(),
        )
        .unwrap_err()
    );
    init_dir.close().unwrap();

    let store_dir = tempfile::tempdir().unwrap();
    let store_config = Config {
        data_dir: store_dir.path().join("store"),
        ..Config::default()
    };
    std::fs::create_dir(&store_config.data_dir).unwrap();
    std::fs::create_dir(store_config.data_dir.join("peryx.redb")).unwrap();
    let store_error = format!(
        "{:#}",
        bootstrap_administrator(
            &store_config,
            &args(),
            &mut Cursor::new(b"correct horse battery staple"),
            &mut Vec::new(),
        )
        .unwrap_err()
    );
    store_dir.close().unwrap();

    assert!(init_error.contains("initialize data directory"), "{init_error}");
    assert!(store_error.contains("open metadata store"), "{store_error}");
}

fn args() -> BootstrapAdministratorArgs {
    BootstrapAdministratorArgs {
        runtime: RuntimeArgs::default(),
        display_name: "Alice".to_owned(),
        password_stdin: true,
        password_file: None,
    }
}

fn config(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().join("data"),
        ..Config::default()
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
        password_stdin: false,
        password_file: Some(path),
        ..args()
    }
}

struct FailRead;

impl Read for FailRead {
    fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("read failed"))
    }
}
