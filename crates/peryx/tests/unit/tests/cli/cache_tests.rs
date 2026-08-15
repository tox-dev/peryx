use std::path::PathBuf;

use rstest::rstest;

use super::parse;
use crate::cli::{
    CacheCommand, CacheListArgs, CachePurgeCommand, CachePurgeOrphanedBlobsArgs, CachePurgeResourceArgs, Command,
    RuntimeArgs,
};

#[test]
fn test_parse_cache_list_filters() {
    assert_eq!(
        parse(&[
            "peryx",
            "cache",
            "list",
            "--data-dir",
            "/d",
            "--index",
            "artifacts",
            "--resource",
            "resource",
            "--digest",
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
            "--stale",
            "--min-age-secs",
            "60",
            "--min-size-bytes",
            "1024",
        ])
        .command,
        Command::Cache(CacheCommand::List(CacheListArgs {
            runtime: RuntimeArgs {
                data_dir: Some(PathBuf::from("/d")),
                ..RuntimeArgs::default()
            },
            index: Some("artifacts".to_owned()),
            resource: Some("resource".to_owned()),
            digest: Some("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".to_owned()),
            stale: true,
            min_age_secs: Some(60),
            min_size_bytes: Some(1024),
        }))
    );
}

#[rstest]
#[case::list(&["peryx", "cache", "list", "--data-dir", "/list"][..], "/list")]
#[case::size(&["peryx", "cache", "size", "--data-dir", "/size"][..], "/size")]
#[case::fsck(&["peryx", "cache", "fsck", "--data-dir", "/fsck"][..], "/fsck")]
#[case::purge_resource(
    &[
        "peryx",
        "cache",
        "purge",
        "resource",
        "--data-dir",
        "/resource",
        "--index",
        "artifacts",
        "--resource",
        "resource",
    ][..],
    "/resource"
)]
#[case::purge_orphaned_blobs(&["peryx", "cache", "purge", "orphaned-blobs", "--data-dir", "/blobs"][..], "/blobs")]
fn test_cache_commands_expose_runtime_args(#[case] argv: &[&str], #[case] expected: &str) {
    let Command::Cache(command) = parse(argv).command else {
        panic!("expected cache command");
    };
    assert_eq!(command.runtime_args().data_dir, Some(PathBuf::from(expected)));
}

#[test]
fn test_parse_cache_purge_resource_requires_yes_for_mutation() {
    assert_eq!(
        parse(&[
            "peryx",
            "cache",
            "purge",
            "resource",
            "--data-dir",
            "/d",
            "--index",
            "artifacts",
            "--resource",
            "resource",
        ])
        .command,
        Command::Cache(CacheCommand::Purge(CachePurgeCommand::Resource(
            CachePurgeResourceArgs {
                runtime: RuntimeArgs {
                    data_dir: Some(PathBuf::from("/d")),
                    ..RuntimeArgs::default()
                },
                index: "artifacts".to_owned(),
                resource: "resource".to_owned(),
                yes: false,
            },
        )))
    );
}

#[test]
fn test_parse_cache_purge_orphaned_blobs_confirmation() {
    assert_eq!(
        parse(&["peryx", "cache", "purge", "orphaned-blobs", "--data-dir", "/d", "--yes"]).command,
        Command::Cache(CacheCommand::Purge(CachePurgeCommand::OrphanedBlobs(
            CachePurgeOrphanedBlobsArgs {
                runtime: RuntimeArgs {
                    data_dir: Some(PathBuf::from("/d")),
                    ..RuntimeArgs::default()
                },
                yes: true,
            },
        )))
    );
}
