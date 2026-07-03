use crate::app::{self, init_data_dir};
use crate::config::Config;

#[test]
fn test_init_data_dir_creates_then_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("data");
    assert!(init_data_dir(&target).unwrap());
    assert!(!init_data_dir(&target).unwrap());
    assert!(target.is_dir());
}

#[test]
fn test_init_data_dir_errors_when_parent_is_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("blocker");
    std::fs::write(&file, "x").unwrap();
    assert!(init_data_dir(&file.join("sub")).is_err());
}

#[test]
fn test_init_creates_dir() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().join("d"),
        ..Config::default()
    };
    app::init(&config).unwrap();
    assert!(config.data_dir.is_dir());
}

#[test]
fn test_init_error() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("blocker");
    std::fs::write(&file, "x").unwrap();
    let config = Config {
        data_dir: file.join("sub"),
        ..Config::default()
    };
    assert!(app::init(&config).is_err());
}

#[test]
fn test_init_logs_both_branches_when_subscriber_enabled() {
    let subscriber = tracing_subscriber::fmt().with_writer(std::io::sink).finish();
    tracing::subscriber::with_default(subscriber, || {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            data_dir: dir.path().join("d"),
            ..Config::default()
        };
        app::init(&config).unwrap(); // created
        app::init(&config).unwrap(); // already exists
    });
}
