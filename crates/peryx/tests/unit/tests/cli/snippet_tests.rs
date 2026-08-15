use std::path::PathBuf;

use super::parse;
use crate::cli::{Command, ConfigSnippetArgs};

#[test]
fn test_parse_config_snippet() {
    assert_eq!(
        parse(&[
            "peryx",
            "config-snippet",
            "--config",
            "peryx.toml",
            "--base-url",
            "https://artifacts.example",
            "--index",
            "root/artifacts",
            "client.conf",
        ])
        .command,
        Command::ConfigSnippet(ConfigSnippetArgs {
            config: Some(PathBuf::from("peryx.toml")),
            base_url: "https://artifacts.example".to_owned(),
            index: "root/artifacts".to_owned(),
            format: "client.conf".to_owned(),
        })
    );
}
