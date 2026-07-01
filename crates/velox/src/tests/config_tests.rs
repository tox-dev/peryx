use std::path::PathBuf;

use crate::config::{self, Config, LogConfig, LogFormat, LogSink, PartialConfig, PartialLogConfig};

#[test]
fn test_default_config() {
    let c = Config::default();
    assert_eq!(c.host, "127.0.0.1");
    assert_eq!(c.port, 4433);
    assert_eq!(c.data_dir, PathBuf::from("velox-data"));
    assert_eq!(c.upstream_url, "https://pypi.org/simple/");
    assert_eq!(c.log, LogConfig::default());
}

#[test]
fn test_apply_overlays_only_present_fields() {
    let partial = PartialConfig {
        host: Some("0.0.0.0".to_owned()),
        port: Some(9000),
        upstream_url: Some("https://example.test/simple/".to_owned()),
        ..PartialConfig::default()
    };
    let merged = Config::default().apply(partial);
    assert_eq!(merged.host, "0.0.0.0");
    assert_eq!(merged.port, 9000);
    assert_eq!(merged.data_dir, PathBuf::from("velox-data"));
    assert_eq!(merged.upstream_url, "https://example.test/simple/");
}

#[test]
fn test_apply_data_dir_and_log() {
    let partial = PartialConfig {
        data_dir: Some(PathBuf::from("/tmp/velox")),
        log: PartialLogConfig {
            level: Some("debug".to_owned()),
            format: Some(LogFormat::Json),
            sink: Some(LogSink::File),
            file: Some(PathBuf::from("velox.log")),
        },
        ..PartialConfig::default()
    };
    let merged = Config::default().apply(partial);
    assert_eq!(merged.data_dir, PathBuf::from("/tmp/velox"));
    assert_eq!(merged.log.level, "debug");
    assert_eq!(merged.log.format, LogFormat::Json);
    assert_eq!(merged.log.sink, LogSink::File);
    assert_eq!(merged.log.file, Some(PathBuf::from("velox.log")));
}

#[test]
fn test_log_config_apply_empty_keeps_defaults() {
    let base = LogConfig::default();
    assert_eq!(base.clone().apply(PartialLogConfig::default()), base);
}

#[test]
fn test_from_toml_ok() {
    let text = "host = \"1.2.3.4\"\nport = 8080\n[log]\nlevel = \"warn\"\nformat = \"json\"\nsink = \"stdout\"\n";
    let p = config::from_toml(PathBuf::from("x.toml"), text).unwrap();
    assert_eq!(p.host.as_deref(), Some("1.2.3.4"));
    assert_eq!(p.port, Some(8080));
    assert_eq!(p.log.level.as_deref(), Some("warn"));
    assert_eq!(p.log.format, Some(LogFormat::Json));
    assert_eq!(p.log.sink, Some(LogSink::Stdout));
}

#[test]
fn test_from_toml_rejects_unknown_key() {
    let err = config::from_toml(PathBuf::from("bad.toml"), "bogus = 1").unwrap_err();
    assert!(err.to_string().contains("bad.toml"));
}

#[test]
fn test_from_toml_rejects_unknown_log_key() {
    assert!(config::from_toml(PathBuf::from("x.toml"), "[log]\nbogus = 1\n").is_err());
}

#[test]
fn test_from_toml_rejects_invalid_log_format() {
    assert!(config::from_toml(PathBuf::from("x.toml"), "[log]\nformat = \"xml\"\n").is_err());
}

#[test]
fn test_from_toml_rejects_invalid_log_sink() {
    assert!(config::from_toml(PathBuf::from("x.toml"), "[log]\nsink = \"kafka\"\n").is_err());
}

#[test]
fn test_from_env_reads_known_keys_and_ignores_others() {
    let vars = vec![
        ("VELOX_HOST".to_owned(), "10.0.0.1".to_owned()),
        ("VELOX_PORT".to_owned(), "7000".to_owned()),
        ("VELOX_DATA_DIR".to_owned(), "/data".to_owned()),
        ("VELOX_UPSTREAM_URL".to_owned(), "https://up/simple/".to_owned()),
        ("VELOX_LOG_LEVEL".to_owned(), "trace".to_owned()),
        ("UNRELATED".to_owned(), "ignored".to_owned()),
    ];
    let p = config::from_env(vars).unwrap();
    assert_eq!(p.host.as_deref(), Some("10.0.0.1"));
    assert_eq!(p.port, Some(7000));
    assert_eq!(p.data_dir, Some(PathBuf::from("/data")));
    assert_eq!(p.upstream_url.as_deref(), Some("https://up/simple/"));
    assert_eq!(p.log.level.as_deref(), Some("trace"));
}

#[test]
fn test_from_env_invalid_port_errors() {
    let err = config::from_env(vec![("VELOX_PORT".to_owned(), "not-a-number".to_owned())]).unwrap_err();
    assert!(err.to_string().contains("VELOX_PORT"));
}

#[test]
fn test_from_file_ok() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("velox.toml");
    std::fs::write(&path, "port = 1234\n").unwrap();
    assert_eq!(config::from_file(path).unwrap().port, Some(1234));
}

#[test]
fn test_from_file_missing_errors() {
    let dir = tempfile::tempdir().unwrap();
    let err = config::from_file(dir.path().join("nope.toml")).unwrap_err();
    assert!(err.to_string().contains("nope.toml"));
}
