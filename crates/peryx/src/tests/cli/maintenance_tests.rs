use std::path::PathBuf;

use super::parse;
use crate::cli::{BackupCommand, Command, PolicyCommand};

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
