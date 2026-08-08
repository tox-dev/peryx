use clap::Parser as _;

use crate::cli::{Cli, Command, RevocationCommand, RevocationStatusArg};

#[test]
fn test_parse_revocation_put_with_stdin_password() {
    let cli = Cli::try_parse_from([
        "peryx",
        "revocation",
        "put",
        "--server",
        "https://packages.example",
        "--user",
        "Alice",
        "--password-stdin",
        "--reason",
        "incident",
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    ])
    .unwrap();
    let Command::Revocation(RevocationCommand::Put(args)) = cli.command else {
        panic!("expected revocation put");
    };

    assert_eq!(args.client.server, "https://packages.example");
    assert_eq!(args.client.user, "Alice");
    assert!(args.client.password_stdin);
    assert_eq!(args.reason, "incident");
}

#[test]
fn test_parse_revocation_list_with_password_file() {
    let cli = Cli::try_parse_from([
        "peryx",
        "revocation",
        "list",
        "--server",
        "https://packages.example",
        "--user",
        "Alice",
        "--password-file",
        "/run/secrets/peryx-password",
        "--status",
        "active",
        "--limit",
        "10",
    ])
    .unwrap();
    let Command::Revocation(RevocationCommand::List(args)) = cli.command else {
        panic!("expected revocation list");
    };

    assert_eq!(args.status, Some(RevocationStatusArg::Active));
    assert_eq!(args.limit, Some(10));
    assert_eq!(
        args.client.password_file.unwrap().to_str().unwrap(),
        "/run/secrets/peryx-password"
    );
}

#[test]
fn test_parse_revocation_requires_exactly_one_password_source() {
    let base = [
        "peryx",
        "revocation",
        "inspect",
        "--server",
        "https://packages.example",
        "--user",
        "Alice",
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    ];
    assert!(Cli::try_parse_from(base).is_err());
    let mut both = base.to_vec();
    both.splice(7..7, ["--password-stdin", "--password-file", "/run/secrets/password"]);
    assert!(Cli::try_parse_from(both).is_err());
}
