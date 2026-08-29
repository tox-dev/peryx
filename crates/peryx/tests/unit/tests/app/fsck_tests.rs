use peryx_storage::blob::{BlobStore, Digest};
use peryx_storage::meta::MetaStore;

use crate::app::cache_with_plugins;
use crate::app::tests::{bounded_output, config_at, plugins, runtime_args, write_invalid_blob_path};
use crate::cli::{CacheCommand, CacheRuntimeArgs};

#[test]
fn test_cache_fsck_reports_a_valid_store() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_at(&dir);
    MetaStore::open(config.data_dir.join("peryx.redb")).unwrap();
    BlobStore::new(config.data_dir.join("blobs")).write(b"valid").unwrap();
    let mut output = Vec::new();

    cache_with_plugins(&config, &plugins(), &command(), &mut output).unwrap();

    assert_eq!(output, b"ok\n");
}

#[test]
fn test_cache_fsck_includes_plugin_metadata_problems() {
    let dir = tempfile::tempdir().unwrap();
    let plugins = crate::tests::support::plugins_with_fsck();
    let config = crate::config::Config {
        data_dir: dir.path().to_path_buf(),
        ..crate::config::Config::with_plugins(&plugins)
    };
    MetaStore::open(config.data_dir.join("peryx.redb")).unwrap();
    let mut output = Vec::new();

    cache_with_plugins(&config, &plugins, &command(), &mut output).unwrap();

    assert_eq!(output, b"metadata\tcore\tinvalid\nproblems\t1\n");
}

#[test]
fn test_cache_fsck_reports_an_ecosystem_without_a_checker() {
    let dir = tempfile::tempdir().unwrap();
    let config = config_at(&dir);
    let meta = MetaStore::open(config.data_dir.join("peryx.redb")).unwrap();
    crate::tests::support::store_repositories(&meta, &["missing"]);
    drop(meta);
    let mut output = Vec::new();

    cache_with_plugins(&config, &plugins(), &command(), &mut output).unwrap();

    assert_eq!(output, b"metadata\tmissing\tmissing checker\nproblems\t1\n");
}

#[test]
fn test_cache_fsck_propagates_plugin_output_failures() {
    let dir = tempfile::tempdir().unwrap();
    let plugins = crate::tests::support::plugins_with_fsck();
    let config = crate::config::Config {
        data_dir: dir.path().to_path_buf(),
        ..crate::config::Config::with_plugins(&plugins)
    };
    MetaStore::open(config.data_dir.join("peryx.redb")).unwrap();

    let error = cache_with_plugins(&config, &plugins, &command(), &mut bounded_output(0)).unwrap_err();

    assert_eq!(
        format!("{error:#}"),
        "fsck ecosystem metadata: failed to write whole buffer"
    );
}

#[test]
fn test_cache_fsck_reports_a_blob_hash_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let digest = Digest::of(b"expected");
    let path = blobs.path_for(&digest);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"tampered").unwrap();
    let mut output = Vec::new();

    cache_with_plugins(&config_at(&dir), &plugins(), &command(), &mut output).unwrap();

    assert_eq!(
        String::from_utf8(output).unwrap(),
        format!("blob\thash\t{}\tdigest mismatch\nproblems\t1\n", digest.as_str())
    );
}

#[test]
fn test_cache_fsck_reports_an_invalid_blob_path() {
    let dir = tempfile::tempdir().unwrap();
    MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    write_invalid_blob_path(dir.path());
    let mut output = Vec::new();

    cache_with_plugins(&config_at(&dir), &plugins(), &command(), &mut output).unwrap();

    assert!(
        String::from_utf8(output)
            .unwrap()
            .contains("invalid content-addressed path")
    );
}

#[cfg(unix)]
#[test]
fn test_cache_fsck_reports_a_blob_read_error() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let digest = Digest::of(b"blocked");
    let path = blobs.path_for(&digest);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"blocked").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    let mut output = Vec::new();

    cache_with_plugins(&config_at(&dir), &plugins(), &command(), &mut output).unwrap();

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(String::from_utf8(output).unwrap().contains("blob\tread\t"));
}

#[test]
fn test_cache_fsck_propagates_output_failures() {
    let dir = tempfile::tempdir().unwrap();
    MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    write_invalid_blob_path(dir.path());

    let error = cache_with_plugins(&config_at(&dir), &plugins(), &command(), &mut bounded_output(0)).unwrap_err();

    assert!(error.to_string().contains("scan blob files"), "{error:#}");
}

const fn command() -> CacheCommand {
    CacheCommand::Fsck(CacheRuntimeArgs {
        runtime: runtime_args(),
    })
}
