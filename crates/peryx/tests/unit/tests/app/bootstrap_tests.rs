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

#[tokio::test]
async fn test_provision_initial_administrator_creates_an_authenticating_admin() {
    let dir = tempfile::tempdir().unwrap();
    let (config, state) = serving(&dir, false);

    let path = provision_initial_administrator(&config, &state).await.unwrap().unwrap();

    assert_eq!(path, config.data_dir.join("initial-admin-password"));
    let password = std::fs::read_to_string(&path).unwrap();
    assert!((15..=1_024).contains(&password.chars().count()), "{}", password.len());
    let users = UserService::new(state.serving.meta.clone());
    let admin = users.identify("admin").unwrap().unwrap();
    assert_eq!(users.authenticate("admin", &password).await.unwrap(), Some(admin.id));
}

#[cfg(unix)]
#[tokio::test]
async fn test_provision_initial_administrator_writes_an_owner_only_file() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let (config, state) = serving(&dir, false);

    let path = provision_initial_administrator(&config, &state).await.unwrap().unwrap();

    assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o600);
}

#[tokio::test]
async fn test_provision_initial_administrator_draws_a_fresh_password_per_store() {
    let (first, second) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (first_config, first_state) = serving(&first, false);
    let (second_config, second_state) = serving(&second, false);

    let first_path = provision_initial_administrator(&first_config, &first_state)
        .await
        .unwrap()
        .unwrap();
    let second_path = provision_initial_administrator(&second_config, &second_state)
        .await
        .unwrap()
        .unwrap();

    assert_ne!(
        std::fs::read_to_string(first_path).unwrap(),
        std::fs::read_to_string(second_path).unwrap()
    );
}

#[tokio::test]
async fn test_provision_initial_administrator_logs_the_path_but_not_the_password() {
    let dir = tempfile::tempdir().unwrap();
    let (config, state) = serving(&dir, false);
    let mut log = tempfile::tempfile().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(log.try_clone().unwrap()))
        .finish();

    let path = {
        let _guard = tracing::subscriber::set_default(subscriber);
        provision_initial_administrator(&config, &state).await.unwrap().unwrap()
    };

    let mut text = String::new();
    std::io::Seek::rewind(&mut log).unwrap();
    log.read_to_string(&mut text).unwrap();
    let password = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains(&path.display().to_string()), "{text}");
    assert!(!text.contains(&password), "{text}");
}

#[tokio::test]
async fn test_provision_initial_administrator_leaves_an_existing_admin_and_file_alone() {
    let dir = tempfile::tempdir().unwrap();
    let (config, state) = serving(&dir, false);
    let path = provision_initial_administrator(&config, &state).await.unwrap().unwrap();
    let password = std::fs::read_to_string(&path).unwrap();

    assert_eq!(provision_initial_administrator(&config, &state).await.unwrap(), None);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), password);
}

#[tokio::test]
async fn test_provision_initial_administrator_skips_an_explicitly_bootstrapped_store() {
    let dir = tempfile::tempdir().unwrap();
    let (config, state) = serving(&dir, false);
    UserService::new(state.serving.meta.clone())
        .bootstrap_administrator("Alice", "correct horse battery staple")
        .await
        .unwrap();

    assert_eq!(provision_initial_administrator(&config, &state).await.unwrap(), None);
    assert!(!config.data_dir.join("initial-admin-password").exists());
    assert_eq!(
        UserService::new(state.serving.meta.clone()).identify("admin").unwrap(),
        None
    );
}

#[rstest]
#[case::dc(AvailabilityConfig::Dc(primary()))]
#[case::ha(AvailabilityConfig::Ha(primary()))]
#[tokio::test]
async fn test_provision_initial_administrator_skips_replicated_modes(#[case] availability: AvailabilityConfig) {
    let dir = tempfile::tempdir().unwrap();
    let (config, state) = serving(&dir, false);
    let config = Config { availability, ..config };

    assert_eq!(provision_initial_administrator(&config, &state).await.unwrap(), None);
    assert!(!config.data_dir.join("initial-admin-password").exists());
    assert!(!state.serving.meta.administrator_exists().unwrap());
}

#[tokio::test]
async fn test_provision_initial_administrator_skips_a_read_only_server() {
    let dir = tempfile::tempdir().unwrap();
    let (config, state) = serving(&dir, true);

    assert_eq!(provision_initial_administrator(&config, &state).await.unwrap(), None);
    assert!(!config.data_dir.join("initial-admin-password").exists());
}

#[tokio::test]
async fn test_provision_initial_administrator_refuses_to_overwrite_a_stale_password_file() {
    let dir = tempfile::tempdir().unwrap();
    let (config, state) = serving(&dir, false);
    let path = config.data_dir.join("initial-admin-password");
    std::fs::write(&path, "stale").unwrap();

    let error = provision_initial_administrator(&config, &state).await.unwrap_err();

    assert!(
        format!("{error:#}").contains(&format!("write initial administrator password {}", path.display())),
        "{error:#}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "stale");
    assert!(!state.serving.meta.administrator_exists().unwrap());
}

#[tokio::test]
async fn test_provision_initial_administrator_removes_the_file_when_bootstrap_fails() {
    let dir = tempfile::tempdir().unwrap();
    let (config, state) = serving(&dir, false);
    UserService::new(state.serving.meta.clone()).create("admin").unwrap();

    let error = provision_initial_administrator(&config, &state).await.unwrap_err();

    assert!(
        format!("{error:#}").contains("create initial administrator \"admin\": user identity \"admin\" already exists"),
        "{error:#}"
    );
    assert!(!config.data_dir.join("initial-admin-password").exists());
}

fn serving(dir: &tempfile::TempDir, read_only: bool) -> (Config, std::sync::Arc<peryx_driver::AppState>) {
    let plugins = crate::tests::support::plugins();
    let config = Config {
        data_dir: dir.path().join("data"),
        read_only,
        ..Config::with_plugins(&plugins)
    };
    let active = crate::server::activate_plugins(&config, &plugins).unwrap();
    let state = crate::server::build_state_with_active_plugins(&config, &active).unwrap();
    (config, state)
}

fn primary() -> crate::config::ReplicationConfig {
    crate::config::ReplicationConfig::Primary {
        source: "writer".to_owned(),
        token: crate::config::SecretSource::Literal("replication-token".to_owned()),
    }
}
