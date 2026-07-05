use std::path::PathBuf;

use clap::Parser as _;
use velodex_ecosystem_pypi::discovery::SnippetKind;

use crate::cli::{
    BackupCommand, CacheCommand, CachePurgeCommand, Cli, Command, PolicyCommand, PrefetchCommand, RuntimeArgs,
    SnippetFormat,
};
use crate::config::{LogFormat, LogSink, PrefetchMode};

fn parse(args: &[&str]) -> Cli {
    Cli::try_parse_from(args).unwrap()
}

fn runtime(cli: Cli) -> RuntimeArgs {
    match cli.command {
        Command::Serve(args) | Command::Init(args) => args,
        Command::ConfigSnippet(_) => panic!("no runtime args on config-snippet"),
        Command::Cache(_) => panic!("cache commands carry nested runtime args"),
        Command::Backup(_) => panic!("backup commands carry nested runtime args"),
        Command::Restore(_) => panic!("restore takes explicit data-dir args"),
        Command::ImportDir(_) => panic!("import-dir carries nested runtime args"),
        Command::Policy(_) => panic!("policy commands carry nested runtime args"),
        Command::Prefetch(_) => panic!("mirror commands carry nested runtime args"),
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
        "--offline",
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
    assert_eq!(o.offline, Some(true));
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

#[test]
fn test_parse_cache_list_filters() {
    let cli = parse(&[
        "velodex",
        "cache",
        "list",
        "--data-dir",
        "/d",
        "--index",
        "pypi",
        "--project",
        "Flask",
        "--digest",
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
        "--stale",
        "--min-age-secs",
        "60",
        "--min-size-bytes",
        "1024",
    ]);
    let Command::Cache(CacheCommand::List(args)) = cli.command else {
        panic!("expected cache list");
    };
    assert_eq!(args.runtime.data_dir, Some(PathBuf::from("/d")));
    assert_eq!(args.index.as_deref(), Some("pypi"));
    assert_eq!(args.project.as_deref(), Some("Flask"));
    assert_eq!(
        args.digest.as_deref(),
        Some("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824")
    );
    assert!(args.stale);
    assert_eq!(args.min_age_secs, Some(60));
    assert_eq!(args.min_size_bytes, Some(1024));
}

#[test]
fn test_parse_cache_size_and_fsck() {
    let size = parse(&["velodex", "cache", "size", "--data-dir", "/d"]);
    let Command::Cache(CacheCommand::Size(args)) = size.command else {
        panic!("expected cache size");
    };
    assert_eq!(args.runtime.data_dir, Some(PathBuf::from("/d")));

    let fsck = parse(&["velodex", "cache", "fsck", "--data-dir", "/d"]);
    let Command::Cache(CacheCommand::Fsck(args)) = fsck.command else {
        panic!("expected cache fsck");
    };
    assert_eq!(args.runtime.data_dir, Some(PathBuf::from("/d")));
}

#[test]
fn test_cache_commands_expose_runtime_args() {
    let list = parse(&["velodex", "cache", "list", "--data-dir", "/list"]);
    let Command::Cache(list) = list.command else {
        panic!("expected cache list");
    };
    assert_eq!(list.runtime_args().data_dir, Some(PathBuf::from("/list")));

    let size = parse(&["velodex", "cache", "size", "--data-dir", "/size"]);
    let Command::Cache(size) = size.command else {
        panic!("expected cache size");
    };
    assert_eq!(size.runtime_args().data_dir, Some(PathBuf::from("/size")));

    let fsck = parse(&["velodex", "cache", "fsck", "--data-dir", "/fsck"]);
    let Command::Cache(fsck) = fsck.command else {
        panic!("expected cache fsck");
    };
    assert_eq!(fsck.runtime_args().data_dir, Some(PathBuf::from("/fsck")));

    let project = parse(&[
        "velodex",
        "cache",
        "purge",
        "project",
        "--data-dir",
        "/project",
        "--index",
        "pypi",
        "--project",
        "Flask",
    ]);
    let Command::Cache(project) = project.command else {
        panic!("expected project purge");
    };
    assert_eq!(project.runtime_args().data_dir, Some(PathBuf::from("/project")));

    let blobs = parse(&["velodex", "cache", "purge", "orphaned-blobs", "--data-dir", "/blobs"]);
    let Command::Cache(blobs) = blobs.command else {
        panic!("expected orphaned blob purge");
    };
    assert_eq!(blobs.runtime_args().data_dir, Some(PathBuf::from("/blobs")));
}

#[test]
fn test_parse_backup_commands() {
    let create = parse(&["velodex", "backup", "create", "--data-dir", "/d", "/backups/velodex"]);
    let Command::Backup(BackupCommand::Create(args)) = create.command else {
        panic!("expected backup create");
    };
    assert_eq!(args.runtime.data_dir, Some(PathBuf::from("/d")));
    assert_eq!(args.path, PathBuf::from("/backups/velodex"));

    let verify = parse(&["velodex", "backup", "verify", "/backups/velodex"]);
    let Command::Backup(BackupCommand::Verify(args)) = verify.command else {
        panic!("expected backup verify");
    };
    assert_eq!(args.path, PathBuf::from("/backups/velodex"));
}

#[test]
fn test_backup_runtime_args_only_apply_to_create() {
    let create = parse(&["velodex", "backup", "create", "--data-dir", "/d", "/backup"]);
    let Command::Backup(create) = create.command else {
        panic!("expected backup create");
    };
    assert_eq!(
        create.runtime_args().and_then(|args| args.data_dir.clone()),
        Some(PathBuf::from("/d"))
    );

    let verify = parse(&["velodex", "backup", "verify", "/backup"]);
    let Command::Backup(verify) = verify.command else {
        panic!("expected backup verify");
    };
    assert!(verify.runtime_args().is_none());
}

#[test]
fn test_parse_restore() {
    let cli = parse(&[
        "velodex",
        "restore",
        "/backups/velodex",
        "--data-dir",
        "/var/lib/velodex",
        "--force",
    ]);
    let Command::Restore(args) = cli.command else {
        panic!("expected restore");
    };
    assert_eq!(args.path, PathBuf::from("/backups/velodex"));
    assert_eq!(args.data_dir, PathBuf::from("/var/lib/velodex"));
    assert!(args.force);
}

#[test]
fn test_parse_import_dir() {
    let cli = parse(&["velodex", "import-dir", "--data-dir", "/d", "root/pypi", "/packages"]);
    let Command::ImportDir(args) = cli.command else {
        panic!("expected import-dir");
    };
    assert_eq!(args.runtime.data_dir, Some(PathBuf::from("/d")));
    assert_eq!(args.index, "root/pypi");
    assert_eq!(args.dir, PathBuf::from("/packages"));
}

#[test]
fn test_parse_policy_dry_run_filters() {
    let cli = parse(&[
        "velodex",
        "policy",
        "dry-run",
        "--data-dir",
        "/d",
        "--index",
        "root/pypi",
        "--project",
        "Flask",
    ]);
    let Command::Policy(PolicyCommand::DryRun(args)) = cli.command else {
        panic!("expected policy dry-run");
    };
    assert_eq!(args.runtime.data_dir, Some(PathBuf::from("/d")));
    assert_eq!(args.index.as_deref(), Some("root/pypi"));
    assert_eq!(args.project.as_deref(), Some("Flask"));
}

#[test]
fn test_policy_commands_expose_runtime_args() {
    let cli = parse(&["velodex", "policy", "dry-run", "--data-dir", "/policy"]);
    let Command::Policy(command) = cli.command else {
        panic!("expected policy dry-run");
    };
    assert_eq!(command.runtime_args().data_dir, Some(PathBuf::from("/policy")));
}

#[test]
fn test_parse_mirror_plan_options() {
    let cli = parse(&[
        "velodex",
        "prefetch",
        "plan",
        "--data-dir",
        "/d",
        "--offline",
        "root/pypi",
        "--package",
        "Requests>=2,<3",
        "--requirements",
        "requirements.txt",
        "--mode",
        "metadata-only",
        "--metadata-only",
        "--no-wheels",
        "--no-sdists",
        "--python-tag",
        "py3",
        "--abi-tag",
        "none",
        "--platform-tag",
        "any",
        "--max-file-size-bytes",
        "1024",
    ]);
    let Command::Prefetch(PrefetchCommand::Plan(args)) = cli.command else {
        panic!("expected mirror plan");
    };
    assert_eq!(args.options.runtime.data_dir, Some(PathBuf::from("/d")));
    assert!(args.options.runtime.offline);
    assert_eq!(args.options.index, "root/pypi");
    assert_eq!(args.options.packages, vec!["Requests>=2,<3".to_owned()]);
    assert_eq!(args.options.requirements, vec![PathBuf::from("requirements.txt")]);
    assert_eq!(args.options.mode, Some(PrefetchMode::MetadataOnly));
    assert!(args.options.metadata_only);
    assert!(args.options.no_wheels);
    assert!(args.options.no_sdists);
    assert_eq!(args.options.python_tags, vec!["py3".to_owned()]);
    assert_eq!(args.options.abi_tags, vec!["none".to_owned()]);
    assert_eq!(args.options.platform_tags, vec!["any".to_owned()]);
    assert_eq!(args.options.max_file_size_bytes, Some(1024));
}

#[test]
fn test_mirror_commands_expose_runtime_args() {
    for cli in [
        parse(&["velodex", "prefetch", "plan", "--data-dir", "/plan", "pypi"]),
        parse(&["velodex", "prefetch", "sync", "--data-dir", "/sync", "pypi"]),
        parse(&["velodex", "prefetch", "verify", "--data-dir", "/verify", "pypi"]),
    ] {
        let Command::Prefetch(command) = cli.command else {
            panic!("expected mirror command");
        };
        assert!(command.runtime_args().data_dir.is_some());
    }
}

#[test]
fn test_parse_cache_purge_project_requires_yes_for_mutation() {
    let cli = parse(&[
        "velodex",
        "cache",
        "purge",
        "project",
        "--data-dir",
        "/d",
        "--index",
        "pypi",
        "--project",
        "Flask",
    ]);
    let Command::Cache(CacheCommand::Purge(CachePurgeCommand::Project(args))) = cli.command else {
        panic!("expected cache purge project");
    };
    assert_eq!(args.runtime.data_dir, Some(PathBuf::from("/d")));
    assert_eq!(args.index, "pypi");
    assert_eq!(args.project, "Flask");
    assert!(!args.yes);
}

#[test]
fn test_parse_cache_purge_orphaned_blobs_confirmation() {
    let cli = parse(&[
        "velodex",
        "cache",
        "purge",
        "orphaned-blobs",
        "--data-dir",
        "/d",
        "--yes",
    ]);
    let Command::Cache(CacheCommand::Purge(CachePurgeCommand::OrphanedBlobs(args))) = cli.command else {
        panic!("expected cache purge orphaned-blobs");
    };
    assert_eq!(args.runtime.data_dir, Some(PathBuf::from("/d")));
    assert!(args.yes);
}
