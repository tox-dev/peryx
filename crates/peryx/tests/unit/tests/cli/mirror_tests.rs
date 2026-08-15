use std::path::PathBuf;

use rstest::rstest;

use super::parse;
use crate::cli::{Command, PrefetchCommand, PrefetchOptions, PrefetchPlanArgs, RuntimeArgs};

#[test]
fn test_parse_prefetch_plan_options() {
    assert_eq!(
        parse(&[
            "peryx",
            "mirror",
            "plan",
            "--data-dir",
            "/d",
            "--offline",
            "root/artifacts",
            "--option",
            "mode='all'",
        ])
        .command,
        Command::Prefetch(PrefetchCommand::Plan(PrefetchPlanArgs {
            options: PrefetchOptions {
                runtime: RuntimeArgs {
                    data_dir: Some(PathBuf::from("/d")),
                    offline: true,
                    ..RuntimeArgs::default()
                },
                index: "root/artifacts".to_owned(),
                overrides: vec!["mode='all'".to_owned()],
            },
        }))
    );
}

#[rstest]
#[case::plan(&["peryx", "mirror", "plan", "--data-dir", "/plan", "artifacts"], "/plan")]
#[case::sync(&["peryx", "mirror", "sync", "--data-dir", "/sync", "artifacts"], "/sync")]
#[case::verify(&["peryx", "mirror", "verify", "--data-dir", "/verify", "artifacts"], "/verify")]
fn test_prefetch_commands_expose_runtime_args(#[case] argv: &[&str], #[case] expected: &str) {
    let Command::Prefetch(command) = parse(argv).command else {
        panic!("expected mirror command");
    };
    assert_eq!(command.runtime_args().data_dir, Some(PathBuf::from(expected)));
}
