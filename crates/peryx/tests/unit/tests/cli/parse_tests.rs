use std::path::PathBuf;

use clap::Parser as _;

use super::parse;
use crate::cli::{Cli, Command, QuotaCommand, RetentionCommand, RuntimeArgs};
use crate::config::{LogFormat, LogSink};

fn runtime(cli: Cli) -> RuntimeArgs {
    match cli.command {
        Command::Serve(args) | Command::Init(args) => args,
        Command::BootstrapAdministrator(_) => panic!("bootstrap-administrator carries nested runtime args"),
        Command::Config(_) => panic!("config commands carry nested runtime args"),
        Command::Revocation(_) => panic!("revocation commands use remote administrator arguments"),
        Command::ConfigSnippet(_) => panic!("no runtime args on config-snippet"),
        Command::Index(_) => panic!("index commands carry nested runtime args"),
        Command::Job(_) => panic!("job commands carry nested runtime args"),
        Command::Cache(_) => panic!("cache commands carry nested runtime args"),
        Command::Backup(_) => panic!("backup commands carry nested runtime args"),
        Command::Restore(_) => panic!("restore takes explicit data-dir args"),
        Command::ImportDir(_) => panic!("import-dir carries nested runtime args"),
        Command::Policy(_) => panic!("policy commands carry nested runtime args"),
        Command::Quota(_) => panic!("quota commands carry nested runtime args"),
        Command::Retention(_) => panic!("retention commands carry nested runtime args"),
        Command::Writer(_) => panic!("writer commands carry nested runtime args"),
        Command::Prefetch(_) => panic!("prefetch commands carry nested runtime args"),
        other @ Command::Openapi => panic!("no runtime args on {other:?}"),
        #[cfg(feature = "self-update")]
        Command::SelfManage(_) => panic!("self commands take no runtime args"),
    }
}

#[test]
fn test_parse_serve_defaults() {
    let args = runtime(parse(&["peryx", "serve"]));
    assert_eq!(args.verbose, 0);
    let overlay = args.overlay();
    assert!(overlay.host.is_none());
    assert!(overlay.indexes.is_none());
    assert!(overlay.log.level.is_none());
}

#[test]
fn test_parse_init_with_flags() {
    let cli = parse(&[
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
    ]);
    assert!(matches!(cli.command, Command::Init(_)));
    let o = runtime(cli).overlay();
    assert_eq!(o.host.as_deref(), Some("0.0.0.0"));
    assert_eq!(o.port, Some(9));
    assert_eq!(o.data_dir, Some(PathBuf::from("/d")));
    assert_eq!(o.writer_identity.as_deref(), Some("writer-a"));
    assert_eq!(o.node_identity.as_deref(), Some("node-b"));
    assert_eq!(o.offline, Some(true));
    assert_eq!(o.read_only, Some(true));
    assert_eq!(o.log.level.as_deref(), Some("debug"));
    assert_eq!(o.log.format, Some(LogFormat::Json));
    assert_eq!(o.log.sink, Some(LogSink::File));
    assert_eq!(o.log.file, Some(PathBuf::from("v.log")));
}

#[test]
fn test_verbose_maps_to_levels() {
    assert_eq!(
        runtime(parse(&["peryx", "serve", "-v"])).overlay().log.level.as_deref(),
        Some("debug")
    );
    assert_eq!(
        runtime(parse(&["peryx", "serve", "-vv"]))
            .overlay()
            .log
            .level
            .as_deref(),
        Some("trace")
    );
    assert_eq!(
        runtime(parse(&["peryx", "serve", "-vvv"]))
            .overlay()
            .log
            .level
            .as_deref(),
        Some("trace")
    );
}

#[test]
fn test_explicit_log_level_beats_verbose() {
    let cli = parse(&["peryx", "serve", "--log-level", "warn", "-vv"]);
    assert_eq!(runtime(cli).overlay().log.level.as_deref(), Some("warn"));
}

#[test]
fn test_parse_retention_dry_run_and_export_carry_runtime_args() {
    let dry_run = parse(&[
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
        "--data-dir",
        "/d",
    ]);
    let Command::Retention(command @ RetentionCommand::DryRun(_)) = dry_run.command else {
        panic!("expected a dry-run command");
    };
    assert_eq!(
        command.runtime_args().data_dir.as_deref(),
        Some(PathBuf::from("/d").as_path())
    );
    if let RetentionCommand::DryRun(args) = &command {
        assert_eq!(args.index, "hosted");
        assert_eq!(args.limit, Some(5));
        assert_eq!(args.cursor.as_deref(), Some("c"));
    }

    let export = parse(&["peryx", "retention", "export", "--index", "hosted", "--data-dir", "/e"]);
    let Command::Retention(command @ RetentionCommand::Export(_)) = export.command else {
        panic!("expected an export command");
    };
    assert_eq!(
        command.runtime_args().data_dir.as_deref(),
        Some(PathBuf::from("/e").as_path())
    );
}

#[test]
fn test_parse_quota_list_and_inspect_carry_runtime_args() {
    let list = parse(&["peryx", "quota", "list", "--data-dir", "/d"]);
    let Command::Quota(command @ QuotaCommand::List(_)) = list.command else {
        panic!("expected a list command");
    };
    assert_eq!(
        command.runtime_args().data_dir.as_deref(),
        Some(PathBuf::from("/d").as_path())
    );

    let inspect = parse(&["peryx", "quota", "inspect", "--index", "hosted", "--data-dir", "/e"]);
    let Command::Quota(command @ QuotaCommand::Inspect(_)) = inspect.command else {
        panic!("expected an inspect command");
    };
    assert_eq!(
        command.runtime_args().data_dir.as_deref(),
        Some(PathBuf::from("/e").as_path())
    );
    let QuotaCommand::Inspect(args) = command else {
        unreachable!("matched an inspect command above");
    };
    assert_eq!(args.index, "hosted");

    assert!(Cli::try_parse_from(["peryx", "quota", "inspect"]).is_err());
}

#[test]
fn test_openapi_takes_no_runtime_flags() {
    let cli = parse(&["peryx", "openapi"]);
    assert!(matches!(cli.command, Command::Openapi));
    assert!(Cli::try_parse_from(["peryx", "openapi", "--port", "1"]).is_err());
}
