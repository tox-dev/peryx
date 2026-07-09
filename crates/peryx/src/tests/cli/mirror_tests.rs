use std::path::PathBuf;

use super::parse;
use crate::cli::{Command, PrefetchCommand};
use crate::config::PrefetchMode;

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
        panic!("expected prefetch plan");
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
