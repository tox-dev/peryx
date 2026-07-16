use std::path::PathBuf;

use rstest::rstest;

use super::env_partial;
use crate::config::{self, LogFormat, LogSink, PartialConfig};

#[test]
fn test_from_file_ok() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.toml");
    std::fs::write(&path, "port = 1234\n").unwrap();
    assert_eq!(config::from_file(path).unwrap().port, Some(1234));
}

#[test]
fn test_from_file_missing_errors() {
    let dir = tempfile::tempdir().unwrap();
    let err = config::from_file(dir.path().join("nope.toml")).unwrap_err();
    assert!(err.to_string().contains("nope.toml"));
}

#[test]
fn test_env_overlays_scalar_and_log_fields() {
    let partial = env_partial(&[
        ("PERYX_HOST", "0.0.0.0"),
        ("PERYX_PORT", "8080"),
        ("PERYX_DATA_DIR", "/srv/peryx"),
        ("PERYX_OFFLINE", "true"),
        ("PERYX_READ_ONLY", "true"),
        ("PERYX_CACHE_TTL_SECS", "42"),
        ("PERYX_HOT_CACHE_BYTES", "1048576"),
        ("PERYX_LOG_LEVEL", "debug"),
        ("PERYX_LOG_FORMAT", "json"),
        ("PERYX_LOG_SINK", "file"),
        ("PERYX_LOG_FILE", "/var/log/peryx.log"),
    ])
    .unwrap();
    assert_eq!(partial.host.as_deref(), Some("0.0.0.0"));
    assert_eq!(partial.port, Some(8080));
    assert_eq!(partial.data_dir, Some(PathBuf::from("/srv/peryx")));
    assert_eq!(partial.offline, Some(true));
    assert_eq!(partial.read_only, Some(true));
    assert_eq!(partial.cache_ttl_secs, Some(42));
    assert_eq!(partial.hot_cache_bytes, Some(1_048_576));
    assert_eq!(partial.log.level.as_deref(), Some("debug"));
    assert_eq!(partial.log.format, Some(LogFormat::Json));
    assert_eq!(partial.log.sink, Some(LogSink::File));
    assert_eq!(partial.log.file, Some(PathBuf::from("/var/log/peryx.log")));
    assert_eq!(partial.indexes, None);
}

#[test]
fn test_env_absent_yields_empty_overlay() {
    assert_eq!(env_partial(&[]).unwrap(), PartialConfig::default());
}

#[test]
fn test_env_empty_string_is_unset() {
    let partial = env_partial(&[("PERYX_HOST", ""), ("PERYX_PORT", "")]).unwrap();
    assert_eq!(partial.host, None);
    assert_eq!(partial.port, None);
}

#[rstest]
#[case::port("PERYX_PORT", "seventy")]
#[case::ttl("PERYX_CACHE_TTL_SECS", "soon")]
#[case::offline("PERYX_OFFLINE", "maybe")]
#[case::read_only("PERYX_READ_ONLY", "maybe")]
#[case::log_format("PERYX_LOG_FORMAT", "xml")]
#[case::log_sink("PERYX_LOG_SINK", "pigeon")]
fn test_env_invalid_is_rejected(#[case] var: &str, #[case] bad_value: &str) {
    let err = env_partial(&[(var, bad_value)]).unwrap_err();
    assert!(err.to_string().contains(var), "{err}");
}

#[test]
fn test_from_env_reads_process_environment() {
    // The process-reading wrapper delegates to the injectable source; with the current environment it
    // must parse without error (no test sets a malformed PERYX_* variable).
    assert!(config::from_env().is_ok());
}
