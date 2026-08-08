use std::path::PathBuf;

use super::parse;
use crate::cli::{Command, PrefetchCommand};

#[test]
fn test_parse_prefetch_plan_options() {
    let cli = parse(&[
        "peryx",
        "mirror",
        "plan",
        "--data-dir",
        "/d",
        "--offline",
        "root/pypi",
        "--option",
        "packages=['Requests>=2,<3']",
        "--option",
        "requirements=['requirements.txt']",
        "--option",
        "mode='metadata-only'",
    ]);
    let Command::Prefetch(PrefetchCommand::Plan(args)) = cli.command else {
        panic!("expected prefetch plan");
    };
    assert_eq!(args.options.runtime.data_dir, Some(PathBuf::from("/d")));
    assert!(args.options.runtime.offline);
    assert_eq!(args.options.index, "root/pypi");
    assert_eq!(
        args.options.overrides,
        [
            "packages=['Requests>=2,<3']",
            "requirements=['requirements.txt']",
            "mode='metadata-only'"
        ]
    );
}

#[test]
fn test_prefetch_commands_expose_runtime_args() {
    for cli in [
        parse(&["peryx", "mirror", "plan", "--data-dir", "/plan", "pypi"]),
        parse(&["peryx", "mirror", "sync", "--data-dir", "/sync", "pypi"]),
        parse(&["peryx", "mirror", "verify", "--data-dir", "/verify", "pypi"]),
    ] {
        let Command::Prefetch(command) = cli.command else {
            panic!("expected prefetch command");
        };
        assert!(command.runtime_args().data_dir.is_some());
    }
}
