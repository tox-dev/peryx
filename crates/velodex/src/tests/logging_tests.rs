use std::path::PathBuf;

use crate::config::{LogConfig, LogSink};
use crate::logging::{self, LogError};

#[test]
fn test_validate_ok_for_stdout() {
    assert_eq!(logging::validate(&LogConfig::default()), Ok(()));
}

#[test]
fn test_validate_file_requires_path() {
    let cfg = LogConfig {
        sink: LogSink::File,
        file: None,
        ..LogConfig::default()
    };
    assert_eq!(logging::validate(&cfg), Err(LogError::MissingFilePath));
}

#[test]
fn test_validate_file_with_path_ok() {
    let cfg = LogConfig {
        sink: LogSink::File,
        file: Some(PathBuf::from("x.log")),
        ..LogConfig::default()
    };
    assert_eq!(logging::validate(&cfg), Ok(()));
}

#[test]
fn test_env_filter_valid() {
    assert!(logging::env_filter("info").is_ok());
    assert!(logging::env_filter("velodex_upstream=debug,info").is_ok());
}

#[test]
fn test_env_filter_invalid() {
    assert!(logging::env_filter("velodex=notalevel").is_err());
}
