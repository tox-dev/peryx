use crate::app::{self, init_data_dir};
use crate::cli::Command;
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
    let file = dir.path().join("afile");
    std::fs::write(&file, "x").unwrap();
    assert!(init_data_dir(&file.join("sub")).is_err());
}

#[test]
fn test_dispatch_init_creates_dir() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().join("d"),
        ..Config::default()
    };
    app::dispatch(Command::Init, &config).unwrap();
    assert!(config.data_dir.is_dir());
}

#[test]
fn test_dispatch_init_existing_dir() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    app::dispatch(Command::Init, &config).unwrap();
}

#[test]
fn test_dispatch_init_error() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("afile");
    std::fs::write(&file, "x").unwrap();
    let config = Config {
        data_dir: file.join("sub"),
        ..Config::default()
    };
    assert!(app::dispatch(Command::Init, &config).is_err());
}

#[test]
fn test_dispatch_serve_ok() {
    app::dispatch(Command::Serve, &Config::default()).unwrap();
}

#[test]
fn test_dispatch_emits_logs_when_subscriber_enabled() {
    // Run every arm with an enabled subscriber so the `tracing::info!` bodies execute.
    let subscriber = tracing_subscriber::fmt().with_writer(std::io::sink).finish();
    tracing::subscriber::with_default(subscriber, || {
        app::dispatch(Command::Serve, &Config::default()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            data_dir: dir.path().join("d"),
            ..Config::default()
        };
        app::dispatch(Command::Init, &config).unwrap();
        app::dispatch(Command::Init, &config).unwrap();
    });
}
