use std::path::PathBuf;

use super::env_partial;
use crate::config::{self, Config, PartialConfig};

#[test]
fn test_env_sits_between_file_and_cli() {
    let resolved = Config::default()
        .apply(config::from_toml(PathBuf::from("x.toml"), "port = 1000\nhost = \"filehost\"\n").unwrap())
        .unwrap()
        .apply(env_partial(&[("VELODEX_PORT", "2000")]).unwrap())
        .unwrap()
        .apply(PartialConfig {
            port: Some(3000),
            ..PartialConfig::default()
        })
        .unwrap();
    assert_eq!(resolved.port, 3000);
    assert_eq!(resolved.host, "filehost");
}

#[test]
fn test_env_overrides_file_when_cli_is_silent() {
    let resolved = Config::default()
        .apply(config::from_toml(PathBuf::from("x.toml"), "port = 1000\n").unwrap())
        .unwrap()
        .apply(env_partial(&[("VELODEX_PORT", "2000")]).unwrap())
        .unwrap();
    assert_eq!(resolved.port, 2000);
}
