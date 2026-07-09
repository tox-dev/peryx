use std::path::PathBuf;

use peryx_ecosystem_pypi::discovery::SnippetKind;

use super::parse;
use crate::cli::{Command, SnippetFormat};

#[test]
fn test_parse_config_snippet() {
    let cli = parse(&[
        "peryx",
        "config-snippet",
        "--config",
        "peryx.toml",
        "--base-url",
        "https://packages.example",
        "--index",
        "root/pypi",
        ".pypirc",
    ]);
    let Command::ConfigSnippet(args) = cli.command else {
        panic!("expected config-snippet");
    };
    assert_eq!(args.config, Some(PathBuf::from("peryx.toml")));
    assert_eq!(args.base_url, "https://packages.example");
    assert_eq!(args.index, "root/pypi");
    assert_eq!(args.format, SnippetFormat::Pypirc);
}

#[test]
fn test_snippet_format_maps_to_discovery_kind() {
    assert_eq!(SnippetKind::from(SnippetFormat::PipConf), SnippetKind::PipConf);
    assert_eq!(SnippetKind::from(SnippetFormat::UvToml), SnippetKind::UvToml);
    assert_eq!(SnippetKind::from(SnippetFormat::Pypirc), SnippetKind::Pypirc);
}
