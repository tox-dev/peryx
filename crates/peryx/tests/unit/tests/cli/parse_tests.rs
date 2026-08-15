use std::path::PathBuf;

use clap::Parser as _;
use rstest::rstest;

use super::parse;
use crate::cli::{Cli, Command, QuotaCommand, QuotaInspectArgs, RetentionCommand, RetentionDryRunArgs, RuntimeArgs};
use crate::config::{LogFormat, LogSink, PartialConfig, PartialLogConfig};

#[test]
fn test_parse_serve_defaults() {
    assert_eq!(
        parse(&["peryx", "serve"]).command,
        Command::Serve(RuntimeArgs::default())
    );
}

#[test]
fn test_parse_init_with_flags() {
    let runtime = RuntimeArgs {
        host: Some("0.0.0.0".to_owned()),
        port: Some(9),
        data_dir: Some(PathBuf::from("/d")),
        writer_identity: Some("writer-a".to_owned()),
        node_identity: Some("node-b".to_owned()),
        offline: true,
        read_only: true,
        log_level: Some("debug".to_owned()),
        log_format: Some(LogFormat::Json),
        log_sink: Some(LogSink::File),
        log_file: Some(PathBuf::from("v.log")),
        ..RuntimeArgs::default()
    };
    assert_eq!(
        parse(&[
            "peryx",
            "init",
            "--host",
            "0.0.0.0",
            "--port",
            "9",
            "--data-dir",
            "/d",
            "--writer-identity",
            "writer-a",
            "--node-identity",
            "node-b",
            "--offline",
            "--read-only",
            "--log-level",
            "debug",
            "--log-format",
            "json",
            "--log-sink",
            "file",
            "--log-file",
            "v.log",
        ])
        .command,
        Command::Init(runtime.clone())
    );
    assert_eq!(
        runtime.overlay(),
        PartialConfig {
            host: Some("0.0.0.0".to_owned()),
            port: Some(9),
            data_dir: Some(PathBuf::from("/d")),
            writer_identity: Some("writer-a".to_owned()),
            node_identity: Some("node-b".to_owned()),
            offline: Some(true),
            read_only: Some(true),
            log: PartialLogConfig {
                level: Some("debug".to_owned()),
                format: Some(LogFormat::Json),
                sink: Some(LogSink::File),
                file: Some(PathBuf::from("v.log")),
            },
            ..PartialConfig::default()
        }
    );
}

#[rstest]
#[case::debug("-v", 1, "debug")]
#[case::trace("-vv", 2, "trace")]
#[case::trace_saturates("-vvv", 3, "trace")]
fn test_verbose_maps_to_level(#[case] flag: &str, #[case] verbose: u8, #[case] expected: &str) {
    let runtime = RuntimeArgs {
        verbose,
        ..RuntimeArgs::default()
    };
    assert_eq!(
        parse(&["peryx", "serve", flag]).command,
        Command::Serve(runtime.clone())
    );
    assert_eq!(runtime.overlay().log.level.as_deref(), Some(expected));
}

#[test]
fn test_explicit_log_level_beats_verbose() {
    let runtime = RuntimeArgs {
        log_level: Some("warn".to_owned()),
        verbose: 2,
        ..RuntimeArgs::default()
    };
    assert_eq!(
        parse(&["peryx", "serve", "--log-level", "warn", "-vv"]).command,
        Command::Serve(runtime.clone())
    );
    assert_eq!(runtime.overlay().log.level.as_deref(), Some("warn"));
}

#[test]
fn test_parse_retention_dry_run_options() {
    assert_eq!(
        parse(&[
            "peryx",
            "retention",
            "dry-run",
            "--index",
            "hosted",
            "--rules",
            "r.toml",
            "--limit",
            "5",
            "--cursor",
            "c",
        ])
        .command,
        Command::Retention(RetentionCommand::DryRun(RetentionDryRunArgs {
            runtime: RuntimeArgs::default(),
            index: "hosted".to_owned(),
            rules: Some(PathBuf::from("r.toml")),
            limit: Some(5),
            cursor: Some("c".to_owned()),
        }))
    );
}

#[rstest]
#[case::dry_run(&["peryx", "retention", "dry-run", "--index", "hosted", "--data-dir", "/dry-run"], "/dry-run")]
#[case::export(&["peryx", "retention", "export", "--index", "hosted", "--data-dir", "/export"], "/export")]
fn test_retention_commands_expose_runtime_args(#[case] argv: &[&str], #[case] expected: &str) {
    let Command::Retention(command) = parse(argv).command else {
        panic!("expected retention command");
    };
    assert_eq!(command.runtime_args().data_dir, Some(PathBuf::from(expected)));
}

#[rstest]
#[case::list(&["peryx", "quota", "list", "--data-dir", "/list"], "/list")]
#[case::inspect(&["peryx", "quota", "inspect", "--index", "hosted", "--data-dir", "/inspect"], "/inspect")]
fn test_quota_commands_expose_runtime_args(#[case] argv: &[&str], #[case] expected: &str) {
    let Command::Quota(command) = parse(argv).command else {
        panic!("expected quota command");
    };
    assert_eq!(command.runtime_args().data_dir, Some(PathBuf::from(expected)));
}

#[test]
fn test_parse_quota_inspect_index() {
    assert_eq!(
        parse(&["peryx", "quota", "inspect", "--index", "hosted"]).command,
        Command::Quota(QuotaCommand::Inspect(QuotaInspectArgs {
            runtime: RuntimeArgs::default(),
            index: "hosted".to_owned(),
        }))
    );
}

#[test]
fn test_parse_quota_inspect_requires_index() {
    assert!(Cli::try_parse_from(["peryx", "quota", "inspect"]).is_err());
}

#[test]
fn test_parse_openapi() {
    assert!(matches!(parse(&["peryx", "openapi"]).command, Command::Openapi));
}

#[test]
fn test_parse_openapi_rejects_runtime_flags() {
    assert!(Cli::try_parse_from(["peryx", "openapi", "--port", "1"]).is_err());
}
