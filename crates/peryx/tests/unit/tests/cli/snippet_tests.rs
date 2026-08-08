use std::path::PathBuf;

use super::parse;
use crate::cli::Command;

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
    assert_eq!(args.format, ".pypirc");
}
