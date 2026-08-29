use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;
use rstest::rstest;

use crate::app;
use crate::app::tests::{bounded_output, runtime_args};
use crate::cli::{CacheCommand, CachePurgeCommand, CachePurgeOrphanedBlobsArgs};
use crate::config::Config;

#[test]
fn test_cache_purge_orphaned_blobs_dry_run_keeps_the_blob() {
    let (_dir, config) = empty_cache();
    let blobs = BlobStore::new(config.data_dir.join("blobs"));
    let orphan = blobs.write(b"orphan").unwrap();
    let mut output = Vec::new();

    app::cache(&config, &orphan_command(false), &mut output).unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(
        output.contains(&format!("dry-run\torphaned-blob\t{}\t6\t", orphan.as_str())),
        "{output}"
    );
    assert!(output.contains("summary\tdry-run\torphaned-blobs\t1\t6\n"), "{output}");
    assert!(output.contains("scope\tecosystems\toci,pypi\n"), "{output}");
    assert!(blobs.exists(&orphan));
}

#[test]
fn test_cache_purge_orphaned_blobs_removes_the_blob_after_confirmation() {
    let (_dir, config) = empty_cache();
    let blobs = BlobStore::new(config.data_dir.join("blobs"));
    let orphan = blobs.write(b"orphan").unwrap();
    let mut output = Vec::new();

    app::cache(&config, &orphan_command(true), &mut output).unwrap();

    let output = String::from_utf8(output).unwrap();
    assert!(
        output.contains(&format!("removed\torphaned-blob\t{}\t6\t", orphan.as_str())),
        "{output}"
    );
    assert!(output.contains("summary\tremoved\torphaned-blobs\t1\t6\n"), "{output}");
    assert!(output.contains("scope\tecosystems\toci,pypi\n"), "{output}");
    assert!(!blobs.exists(&orphan));
}

#[rstest]
#[case::dry_run(false)]
#[case::confirmed(true)]
fn test_cache_purge_refuses_an_uncovered_stored_ecosystem(#[case] yes: bool) {
    let (dir, mut config) = empty_cache();
    config.indexes.retain(|index| index.ecosystem.as_str() == "pypi");
    let meta = MetaStore::open_existing(dir.path().join("peryx.redb")).unwrap();
    crate::tests::support::store_repositories(&meta, &["pypi", "oci"]);
    drop(meta);
    let blobs = BlobStore::new(config.data_dir.join("blobs"));
    let digest = blobs.write(b"must remain").unwrap();
    let mut output = Vec::new();

    let error = app::cache(&config, &orphan_command(yes), &mut output).unwrap_err();

    assert_eq!(
        (error.to_string(), output, blobs.exists(&digest),),
        (
            "scan metadata blob references: metadata contains repositories for ecosystems without blob-reference \
             drivers: oci"
                .to_owned(),
            Vec::new(),
            true,
        )
    );
}

#[test]
fn test_cache_purge_orphaned_blobs_propagates_row_output_failures() {
    let (_dir, config) = empty_cache();
    BlobStore::new(config.data_dir.join("blobs")).write(b"orphan").unwrap();

    let error = app::cache(
        &config,
        &orphan_command(false),
        &mut bounded_output("action\ttarget\tdigest\tsize_bytes\tpath\n".len()),
    )
    .unwrap_err();

    assert!(error.to_string().contains("failed to write whole buffer"), "{error:#}");
}

fn orphan_command(yes: bool) -> CacheCommand {
    CacheCommand::Purge(CachePurgeCommand::OrphanedBlobs(CachePurgeOrphanedBlobsArgs {
        runtime: runtime_args(),
        yes,
    }))
}

fn empty_cache() -> (tempfile::TempDir, Config) {
    let dir = tempfile::tempdir().unwrap();
    drop(MetaStore::open(dir.path().join("peryx.redb")).unwrap());
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    };
    (dir, config)
}
