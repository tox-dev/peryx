use std::path::PathBuf;

use clap::Parser as _;

use super::parse;
use crate::cli::{BackupCommand, Cli, Command, PolicyCommand, WriterCommand};

#[test]
fn test_parse_bootstrap_administrator_secret_sources() {
    let stdin = parse(&[
        "peryx",
        "bootstrap-administrator",
        "Alice",
        "--password-stdin",
        "--data-dir",
        "/data",
    ]);
    let Command::BootstrapAdministrator(stdin) = stdin.command else {
        panic!("expected bootstrap-administrator");
    };
    assert_eq!(stdin.display_name, "Alice");
    assert!(stdin.password_stdin);
    assert_eq!(stdin.password_file, None);
    assert_eq!(stdin.runtime.data_dir, Some(PathBuf::from("/data")));

    let file = parse(&[
        "peryx",
        "bootstrap-administrator",
        "Alice",
        "--password-file",
        "/run/credentials/peryx.service/administrator-password",
    ]);
    let Command::BootstrapAdministrator(file) = file.command else {
        panic!("expected bootstrap-administrator");
    };
    assert_eq!(
        file.password_file,
        Some(PathBuf::from("/run/credentials/peryx.service/administrator-password"))
    );
    assert!(!file.password_stdin);
}

#[test]
fn test_parse_bootstrap_administrator_requires_one_non_argv_secret_source() {
    assert!(Cli::try_parse_from(["peryx", "bootstrap-administrator", "Alice"]).is_err());
    assert!(
        Cli::try_parse_from([
            "peryx",
            "bootstrap-administrator",
            "Alice",
            "--password-stdin",
            "--password-file",
            "/secret"
        ])
        .is_err()
    );
    assert!(
        Cli::try_parse_from([
            "peryx",
            "bootstrap-administrator",
            "Alice",
            "--password",
            "password-must-not-enter-argv"
        ])
        .is_err()
    );
}

#[test]
fn test_parse_writer_promote() {
    let cli = parse(&["peryx", "writer", "promote", "writer-b", "--config", "peryx.toml"]);
    let Command::Writer(WriterCommand::Promote(args)) = cli.command else {
        panic!("expected writer promote");
    };
    assert_eq!(args.runtime.config, Some(PathBuf::from("peryx.toml")));
    assert_eq!(args.replacement, "writer-b");
}

#[test]
fn test_parse_writer_claim() {
    let cli = parse(&["peryx", "writer", "claim", "--data-dir", "/replica"]);
    let Command::Writer(WriterCommand::Claim(args)) = cli.command else {
        panic!("expected writer claim");
    };
    assert_eq!(args.runtime.data_dir, Some(PathBuf::from("/replica")));
}

#[test]
fn test_writer_commands_expose_runtime_args() {
    let promote = parse(&["peryx", "writer", "promote", "writer-b", "--data-dir", "/writer"]);
    let Command::Writer(promote) = promote.command else {
        panic!("expected writer promote");
    };
    assert_eq!(promote.runtime_args().data_dir, Some(PathBuf::from("/writer")));

    let claim = parse(&["peryx", "writer", "claim", "--data-dir", "/replica"]);
    let Command::Writer(claim) = claim.command else {
        panic!("expected writer claim");
    };
    assert_eq!(claim.runtime_args().data_dir, Some(PathBuf::from("/replica")));
}

#[test]
fn test_parse_backup_commands() {
    let create = parse(&["peryx", "backup", "create", "--data-dir", "/d", "/backups/peryx"]);
    let Command::Backup(BackupCommand::Create(args)) = create.command else {
        panic!("expected backup create");
    };
    assert_eq!(args.runtime.data_dir, Some(PathBuf::from("/d")));
    assert_eq!(args.path, PathBuf::from("/backups/peryx"));

    let verify = parse(&["peryx", "backup", "verify", "/backups/peryx"]);
    let Command::Backup(BackupCommand::Verify(args)) = verify.command else {
        panic!("expected backup verify");
    };
    assert_eq!(args.path, PathBuf::from("/backups/peryx"));
}

#[test]
fn test_backup_runtime_args_only_apply_to_create() {
    let create = parse(&["peryx", "backup", "create", "--data-dir", "/d", "/backup"]);
    let Command::Backup(create) = create.command else {
        panic!("expected backup create");
    };
    assert_eq!(
        create.runtime_args().and_then(|args| args.data_dir.clone()),
        Some(PathBuf::from("/d"))
    );

    let verify = parse(&["peryx", "backup", "verify", "/backup"]);
    let Command::Backup(verify) = verify.command else {
        panic!("expected backup verify");
    };
    assert!(verify.runtime_args().is_none());
}

#[test]
fn test_parse_restore() {
    let cli = parse(&[
        "peryx",
        "restore",
        "/backups/peryx",
        "--data-dir",
        "/var/lib/peryx",
        "--force",
    ]);
    let Command::Restore(args) = cli.command else {
        panic!("expected restore");
    };
    assert_eq!(args.path, PathBuf::from("/backups/peryx"));
    assert_eq!(args.data_dir, PathBuf::from("/var/lib/peryx"));
    assert!(args.force);
}

#[test]
fn test_parse_import_dir() {
    let cli = parse(&["peryx", "import-dir", "--data-dir", "/d", "root/pypi", "/packages"]);
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
        "peryx",
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
    let cli = parse(&["peryx", "policy", "dry-run", "--data-dir", "/policy"]);
    let Command::Policy(command) = cli.command else {
        panic!("expected policy dry-run");
    };
    assert_eq!(command.runtime_args().data_dir, Some(PathBuf::from("/policy")));
}
