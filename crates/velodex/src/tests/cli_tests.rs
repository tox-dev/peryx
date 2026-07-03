use std::path::PathBuf;

use clap::Parser as _;
use velodex_http::discovery::SnippetKind;

use crate::cli::{Cli, Command, RuntimeArgs, SnippetFormat};
use crate::config::{LogFormat, LogSink};

fn parse(args: &[&str]) -> Cli {
    Cli::try_parse_from(args).unwrap()
}

fn runtime(cli: Cli) -> RuntimeArgs {
    match cli.command {
        Command::Serve(args) | Command::Init(args) => args,
        Command::ConfigSnippet(_) => panic!("no runtime args on config-snippet"),
        other @ Command::Openapi => panic!("no runtime args on {other:?}"),
    }
}

#[test]
fn test_parse_serve_defaults() {
    let args = runtime(parse(&["velodex", "serve"]));
    assert_eq!(args.verbose, 0);
    let overlay = args.overlay();
    assert!(overlay.host.is_none());
    assert!(overlay.indexes.is_none());
    assert!(overlay.log.level.is_none());
}

#[test]
fn test_parse_init_with_flags() {
    let cli = parse(&[
        "velodex",
        "init",
        "--host",
        "0.0.0.0",
        "--port",
        "9",
        "--data-dir",
        "/d",
        "--log-level",
        "debug",
        "--log-format",
        "json",
        "--log-sink",
        "file",
        "--log-file",
        "v.log",
    ]);
    assert!(matches!(cli.command, Command::Init(_)));
    let o = runtime(cli).overlay();
    assert_eq!(o.host.as_deref(), Some("0.0.0.0"));
    assert_eq!(o.port, Some(9));
    assert_eq!(o.data_dir, Some(PathBuf::from("/d")));
    assert_eq!(o.log.level.as_deref(), Some("debug"));
    assert_eq!(o.log.format, Some(LogFormat::Json));
    assert_eq!(o.log.sink, Some(LogSink::File));
    assert_eq!(o.log.file, Some(PathBuf::from("v.log")));
}

#[test]
fn test_verbose_maps_to_levels() {
    assert_eq!(
        runtime(parse(&["velodex", "serve", "-v"]))
            .overlay()
            .log
            .level
            .as_deref(),
        Some("debug")
    );
    assert_eq!(
        runtime(parse(&["velodex", "serve", "-vv"]))
            .overlay()
            .log
            .level
            .as_deref(),
        Some("trace")
    );
    assert_eq!(
        runtime(parse(&["velodex", "serve", "-vvv"]))
            .overlay()
            .log
            .level
            .as_deref(),
        Some("trace")
    );
}

#[test]
fn test_explicit_log_level_beats_verbose() {
    let cli = parse(&["velodex", "serve", "--log-level", "warn", "-vv"]);
    assert_eq!(runtime(cli).overlay().log.level.as_deref(), Some("warn"));
}

#[test]
fn test_openapi_takes_no_runtime_flags() {
    let cli = parse(&["velodex", "openapi"]);
    assert!(matches!(cli.command, Command::Openapi));
    assert!(Cli::try_parse_from(["velodex", "openapi", "--port", "1"]).is_err());
}

#[test]
fn test_parse_config_snippet() {
    let cli = parse(&[
        "velodex",
        "config-snippet",
        "--config",
        "velodex.toml",
        "--base-url",
        "https://packages.example",
        "--index",
        "root/pypi",
        ".pypirc",
    ]);
    let Command::ConfigSnippet(args) = cli.command else {
        panic!("expected config-snippet");
    };
    assert_eq!(args.config, Some(PathBuf::from("velodex.toml")));
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
