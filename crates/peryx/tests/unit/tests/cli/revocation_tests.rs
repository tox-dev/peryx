use std::path::PathBuf;

use clap::Parser as _;
use rstest::rstest;

use super::parse;
use crate::cli::{
    AdministratorClientArgs, Cli, Command, ListRevocationsArgs, PutRevocationArgs, RevocationCommand,
    RevocationStatusArg,
};

#[rstest]
#[case::put(
    &[
        "peryx",
        "revocation",
        "put",
        "--server",
        "https://put.example",
        "--user",
        "Alice",
        "--password-stdin",
        "--reason",
        "incident",
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    ],
    "https://put.example"
)]
#[case::inspect(
    &[
        "peryx",
        "revocation",
        "inspect",
        "--server",
        "https://inspect.example",
        "--user",
        "Alice",
        "--password-stdin",
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    ],
    "https://inspect.example"
)]
#[case::list(
    &[
        "peryx",
        "revocation",
        "list",
        "--server",
        "https://list.example",
        "--user",
        "Alice",
        "--password-stdin",
    ],
    "https://list.example"
)]
#[case::lift(
    &[
        "peryx",
        "revocation",
        "lift",
        "--server",
        "https://lift.example",
        "--user",
        "Alice",
        "--password-stdin",
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
    ],
    "https://lift.example"
)]
fn test_revocation_commands_expose_their_client(#[case] argv: &[&str], #[case] expected_server: &str) {
    let Command::Revocation(command) = parse(argv).command else {
        panic!("expected revocation command");
    };
    let client = command.client();
    assert_eq!(
        (client.server.as_str(), client.user.as_str()),
        (expected_server, "Alice")
    );
}

#[rstest]
#[case::active(RevocationStatusArg::Active, "active")]
#[case::lifted(RevocationStatusArg::Lifted, "lifted")]
fn test_revocation_status_has_the_api_encoding(#[case] status: RevocationStatusArg, #[case] expected: &str) {
    assert_eq!(status.as_str(), expected);
}

#[test]
fn test_parse_revocation_put_with_stdin_password() {
    let digest = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    assert_eq!(
        parse(&[
            "peryx",
            "revocation",
            "put",
            "--server",
            "https://artifacts.example",
            "--user",
            "Alice",
            "--password-stdin",
            "--reason",
            "incident",
            digest,
        ])
        .command,
        Command::Revocation(RevocationCommand::Put(PutRevocationArgs {
            client: AdministratorClientArgs {
                server: "https://artifacts.example".to_owned(),
                user: "Alice".to_owned(),
                password_stdin: true,
                password_file: None,
            },
            digest: digest.to_owned(),
            reason: "incident".to_owned(),
        }))
    );
}

#[test]
fn test_parse_revocation_list_with_password_file() {
    assert_eq!(
        parse(&[
            "peryx",
            "revocation",
            "list",
            "--server",
            "https://artifacts.example",
            "--user",
            "Alice",
            "--password-file",
            "/run/secrets/peryx-password",
            "--status",
            "active",
            "--limit",
            "10",
        ])
        .command,
        Command::Revocation(RevocationCommand::List(ListRevocationsArgs {
            client: AdministratorClientArgs {
                server: "https://artifacts.example".to_owned(),
                user: "Alice".to_owned(),
                password_stdin: false,
                password_file: Some(PathBuf::from("/run/secrets/peryx-password")),
            },
            status: Some(RevocationStatusArg::Active),
            cursor: None,
            limit: Some(10),
        }))
    );
}

#[rstest]
#[case::missing(&[
    "peryx",
    "revocation",
    "inspect",
    "--server",
    "https://artifacts.example",
    "--user",
    "Alice",
    "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
])]
#[case::conflicting(&[
    "peryx",
    "revocation",
    "inspect",
    "--server",
    "https://artifacts.example",
    "--user",
    "Alice",
    "--password-stdin",
    "--password-file",
    "/run/secrets/password",
    "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
])]
fn test_parse_revocation_requires_exactly_one_password_source(#[case] argv: &[&str]) {
    assert!(Cli::try_parse_from(argv).is_err());
}
