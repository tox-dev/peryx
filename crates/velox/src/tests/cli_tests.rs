use std::path::PathBuf;

use clap::Parser as _;

use crate::cli::{Cli, Command};
use crate::config::{LogFormat, LogSink};

fn parse(args: &[&str]) -> Cli {
    Cli::try_parse_from(args).unwrap()
}

#[test]
fn test_parse_serve_defaults() {
    let cli = parse(&["velox", "serve"]);
    assert_eq!(cli.command, Command::Serve);
    assert_eq!(cli.verbose, 0);
    let overlay = cli.overlay();
    assert!(overlay.host.is_none());
    assert!(overlay.upstream_url.is_none());
    assert!(overlay.log.level.is_none());
}

#[test]
fn test_parse_init_with_flags() {
    let cli = parse(&[
        "velox",
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
        "init",
    ]);
    assert_eq!(cli.command, Command::Init);
    let o = cli.overlay();
    assert_eq!(o.host.as_deref(), Some("0.0.0.0"));
    assert_eq!(o.port, Some(9));
    assert_eq!(o.data_dir, Some(PathBuf::from("/d")));
    assert_eq!(o.log.level.as_deref(), Some("debug"));
    assert_eq!(o.log.format, Some(LogFormat::Json));
    assert_eq!(o.log.sink, Some(LogSink::File));
    assert_eq!(o.log.file, Some(PathBuf::from("v.log")));
}

#[test]
fn test_verbose_maps_to_level() {
    assert_eq!(
        parse(&["velox", "-v", "serve"]).overlay().log.level.as_deref(),
        Some("debug")
    );
    assert_eq!(
        parse(&["velox", "-vv", "serve"]).overlay().log.level.as_deref(),
        Some("trace")
    );
    assert_eq!(
        parse(&["velox", "-vvv", "serve"]).overlay().log.level.as_deref(),
        Some("trace")
    );
}

#[test]
fn test_explicit_log_level_beats_verbose() {
    let cli = parse(&["velox", "--log-level", "warn", "-vv", "serve"]);
    assert_eq!(cli.overlay().log.level.as_deref(), Some("warn"));
}
