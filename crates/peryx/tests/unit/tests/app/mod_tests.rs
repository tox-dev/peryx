use std::io::Cursor;
use std::time::Duration;

use super::*;
use crate::cli::{CacheCommand, CacheRuntimeArgs, RuntimeArgs};
use crate::config::S3StorageConfig;

pub(super) fn bounded_output(capacity: usize) -> Cursor<Box<[u8]>> {
    Cursor::new(vec![0; capacity].into_boxed_slice())
}

pub(super) fn config_at(dir: &tempfile::TempDir) -> Config {
    let config = Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::with_plugins(&plugins())
    };
    drop(peryx_storage::meta::MetaStore::open(config.data_dir.join("peryx.redb")).unwrap());
    config
}

pub(super) const fn runtime_args() -> RuntimeArgs {
    RuntimeArgs {
        config: None,
        host: None,
        port: None,
        data_dir: None,
        writer_identity: None,
        node_identity: None,
        offline: false,
        read_only: false,
        log_level: None,
        verbose: 0,
        log_format: None,
        log_sink: None,
        log_file: None,
    }
}

pub(super) fn write_invalid_blob_path(root: &std::path::Path) {
    let path = root.join("blobs/sha256/aa/bb/not-a-digest");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"x").unwrap();
}

pub(super) use crate::tests::support::{plugins, plugins_without_retention};

fn s3_config() -> S3StorageConfig {
    S3StorageConfig {
        endpoint: "https://s3.example".to_owned(),
        bucket: "peryx".to_owned(),
        prefix: String::new(),
        region: "us-east-1".to_owned(),
        path_style: true,
        request_timeout: Duration::from_secs(30),
        max_retries: 3,
        multipart_threshold: 16 << 20,
        part_size: 16 << 20,
        upload_concurrency: 4,
        conditional_writes: true,
        checksum_writes: true,
    }
}

#[test]
fn test_cache_rejects_an_object_store_before_opening_local_state() {
    let config = Config {
        blob: BlobStorageConfig::S3(s3_config()),
        ..Config::default()
    };
    let error = crate::app::cache(
        &config,
        &CacheCommand::Size(CacheRuntimeArgs {
            runtime: runtime_args(),
        }),
        &mut Vec::new(),
    )
    .unwrap_err();

    assert_eq!(
        error.to_string(),
        "cache maintenance is only supported on the filesystem blob backend, but this repository is configured for S3; run it against a filesystem-backed repository"
    );
}

#[test]
fn test_index_names_are_longest_first_without_losing_entries() {
    let config = Config::default();
    let names = index_names(&config);

    assert!(names.windows(2).all(|pair| pair[0].len() >= pair[1].len()));
    assert_eq!(
        names.iter().copied().collect::<std::collections::BTreeSet<_>>(),
        config
            .indexes
            .iter()
            .map(|index| index.name.as_str())
            .collect::<std::collections::BTreeSet<_>>()
    );
}
