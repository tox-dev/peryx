use std::path::PathBuf;

use rstest::rstest;

use super::parse;
use crate::cli::{CacheCommand, CachePurgeCommand, Command};

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

#[rstest]
#[case::list(&["velodex", "cache", "list", "--data-dir", "/list"][..], "/list")]
#[case::size(&["velodex", "cache", "size", "--data-dir", "/size"][..], "/size")]
#[case::fsck(&["velodex", "cache", "fsck", "--data-dir", "/fsck"][..], "/fsck")]
#[case::purge_project(
    &["velodex", "cache", "purge", "project", "--data-dir", "/project", "--index", "pypi", "--project", "Flask"][..],
    "/project"
)]
#[case::purge_orphaned_blobs(&["velodex", "cache", "purge", "orphaned-blobs", "--data-dir", "/blobs"][..], "/blobs")]
fn test_cache_commands_expose_runtime_args(#[case] argv: &[&str], #[case] expected: &str) {
    let Command::Cache(command) = parse(argv).command else {
        panic!("expected cache command");
    };
    assert_eq!(command.runtime_args().data_dir, Some(PathBuf::from(expected)));
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
