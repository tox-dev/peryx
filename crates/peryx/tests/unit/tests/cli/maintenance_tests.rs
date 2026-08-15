use std::path::PathBuf;

use clap::Parser as _;
use rstest::rstest;

use super::parse;
use crate::cli::{
    BackupCommand, Cli, Command, ImportDirArgs, PolicyCommand, PolicyDryRunArgs, RestoreArgs, RuntimeArgs,
    WriterCommand, WriterPromoteArgs,
};

#[rstest]
#[case::stdin(
    &["peryx", "bootstrap-administrator", "Alice", "--password-stdin", "--data-dir", "/data"],
    true,
    None,
    Some(PathBuf::from("/data"))
)]
#[case::file(
    &[
        "peryx",
        "bootstrap-administrator",
        "Alice",
        "--password-file",
        "/run/credentials/peryx.service/administrator-password",
    ],
    false,
    Some(PathBuf::from("/run/credentials/peryx.service/administrator-password")),
    None
)]
fn test_parse_bootstrap_administrator_secret_source(
    #[case] argv: &[&str],
    #[case] password_stdin: bool,
    #[case] password_file: Option<PathBuf>,
    #[case] data_dir: Option<PathBuf>,
) {
    let Command::BootstrapAdministrator(args) = parse(argv).command else {
        panic!("expected bootstrap administrator command");
    };
    assert_eq!(
        (
            args.display_name.as_str(),
            args.password_stdin,
            args.password_file,
            args.runtime.data_dir,
        ),
        ("Alice", password_stdin, password_file, data_dir)
    );
}

#[rstest]
#[case::missing(&["peryx", "bootstrap-administrator", "Alice"])]
#[case::conflicting(&[
    "peryx",
    "bootstrap-administrator",
    "Alice",
    "--password-stdin",
    "--password-file",
    "/secret",
])]
#[case::password_in_argv(&[
    "peryx",
    "bootstrap-administrator",
    "Alice",
    "--password",
    "password-must-not-enter-argv",
])]
fn test_parse_bootstrap_administrator_rejects_invalid_secret_source(#[case] argv: &[&str]) {
    assert!(Cli::try_parse_from(argv).is_err());
}

#[test]
fn test_parse_writer_promote() {
    assert_eq!(
        parse(&["peryx", "writer", "promote", "writer-b", "--config", "peryx.toml"]).command,
        Command::Writer(WriterCommand::Promote(WriterPromoteArgs {
            runtime: RuntimeArgs {
                config: Some(PathBuf::from("peryx.toml")),
                ..RuntimeArgs::default()
            },
            replacement: "writer-b".to_owned(),
        }))
    );
}

#[rstest]
#[case::promote(&["peryx", "writer", "promote", "writer-b", "--data-dir", "/writer"], "/writer")]
#[case::claim(&["peryx", "writer", "claim", "--data-dir", "/replica"], "/replica")]
fn test_writer_commands_expose_runtime_args(#[case] argv: &[&str], #[case] expected: &str) {
    let Command::Writer(command) = parse(argv).command else {
        panic!("expected writer command");
    };
    assert_eq!(command.runtime_args().data_dir, Some(PathBuf::from(expected)));
}

#[rstest]
#[case::create(&["peryx", "backup", "create", "--data-dir", "/d", "/backups/peryx"], Some(PathBuf::from("/d")))]
#[case::verify(&["peryx", "backup", "verify", "/backups/peryx"], None)]
fn test_parse_backup_command(#[case] argv: &[&str], #[case] data_dir: Option<PathBuf>) {
    let Command::Backup(command) = parse(argv).command else {
        panic!("expected backup command");
    };
    let (actual_data_dir, path) = match command {
        BackupCommand::Create(args) => (args.runtime.data_dir, args.path),
        BackupCommand::Verify(args) => (None, args.path),
    };
    assert_eq!((actual_data_dir, path), (data_dir, PathBuf::from("/backups/peryx")));
}

#[test]
fn test_parse_restore() {
    assert_eq!(
        parse(&[
            "peryx",
            "restore",
            "/backups/peryx",
            "--data-dir",
            "/var/lib/peryx",
            "--force",
        ])
        .command,
        Command::Restore(RestoreArgs {
            path: PathBuf::from("/backups/peryx"),
            data_dir: PathBuf::from("/var/lib/peryx"),
            force: true,
        })
    );
}

#[test]
fn test_parse_import_dir() {
    assert_eq!(
        parse(&[
            "peryx",
            "import-dir",
            "--data-dir",
            "/d",
            "root/artifacts",
            "/artifacts",
        ])
        .command,
        Command::ImportDir(ImportDirArgs {
            runtime: RuntimeArgs {
                data_dir: Some(PathBuf::from("/d")),
                ..RuntimeArgs::default()
            },
            index: "root/artifacts".to_owned(),
            dir: PathBuf::from("/artifacts"),
        })
    );
}

#[test]
fn test_parse_policy_dry_run_filters() {
    assert_eq!(
        parse(&[
            "peryx",
            "policy",
            "dry-run",
            "--data-dir",
            "/d",
            "--index",
            "root/artifacts",
            "--resource",
            "resource",
        ])
        .command,
        Command::Policy(PolicyCommand::DryRun(PolicyDryRunArgs {
            runtime: RuntimeArgs {
                data_dir: Some(PathBuf::from("/d")),
                ..RuntimeArgs::default()
            },
            index: Some("root/artifacts".to_owned()),
            resource: Some("resource".to_owned()),
        }))
    );
}

#[test]
fn test_policy_commands_expose_runtime_args() {
    let command = PolicyCommand::DryRun(PolicyDryRunArgs {
        runtime: RuntimeArgs {
            data_dir: Some(PathBuf::from("/policy")),
            ..RuntimeArgs::default()
        },
        index: None,
        resource: None,
    });
    assert_eq!(command.runtime_args().data_dir, Some(PathBuf::from("/policy")));
}
