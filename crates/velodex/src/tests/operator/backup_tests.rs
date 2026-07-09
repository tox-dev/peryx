use velodex_storage::blob::BlobStore;
use velodex_storage::meta::MetaStore;

use crate::config::{Config, LogConfig, LogFormat, LogSink};
use crate::operator;

use super::backup_fixture;

#[test]
fn test_backup_create_rejects_existing_target_paths() {
    let (_source, config, _content_digest, _metadata_digest) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let file_target = root.path().join("file-backup");
    std::fs::write(&file_target, b"x").unwrap();

    let err = operator::backup_create(&config, &file_target, &mut Vec::new()).unwrap_err();
    assert!(err.to_string().contains("exists and is not a directory"));

    let dir_target = root.path().join("dir-backup");
    std::fs::create_dir(&dir_target).unwrap();
    std::fs::write(dir_target.join("blocker"), b"x").unwrap();
    let err = operator::backup_create(&config, &dir_target, &mut Vec::new()).unwrap_err();
    assert!(err.to_string().contains("is not empty"));
}

#[test]
fn test_backup_create_rejects_missing_source_blob() {
    let (_source, config, content_digest, _metadata_digest) = backup_fixture();
    std::fs::remove_file(BlobStore::new(config.data_dir.join("blobs")).path_for(&content_digest)).unwrap();
    let root = tempfile::tempdir().unwrap();

    let err = operator::backup_create(&config, &root.path().join("backup"), &mut Vec::new()).unwrap_err();

    assert!(err.to_string().contains("referenced blob"));
    assert!(err.to_string().contains("is missing"));
}

#[test]
fn test_backup_create_rejects_tampered_source_blob() {
    let (_source, config, content_digest, _metadata_digest) = backup_fixture();
    std::fs::write(
        BlobStore::new(config.data_dir.join("blobs")).path_for(&content_digest),
        b"tampered",
    )
    .unwrap();
    let root = tempfile::tempdir().unwrap();

    let err = operator::backup_create(&config, &root.path().join("backup"), &mut Vec::new()).unwrap_err();

    assert!(err.to_string().contains("hashed as"));
}

#[test]
fn test_backup_create_snapshots_log_variants() {
    for (format, sink, expected) in [
        (LogFormat::Json, LogSink::File, "format = \"json\"\nsink = \"file\""),
        (
            LogFormat::Pretty,
            LogSink::Journald,
            "format = \"pretty\"\nsink = \"journald\"",
        ),
        (
            LogFormat::Pretty,
            LogSink::Syslog,
            "format = \"pretty\"\nsink = \"syslog\"",
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let data_dir = root.path().join("data");
        std::fs::create_dir(&data_dir).unwrap();
        drop(MetaStore::open(data_dir.join("velodex.redb")).unwrap());
        let backup = root.path().join("backup");

        operator::backup_create(
            &Config {
                data_dir,
                log: LogConfig {
                    format,
                    sink,
                    file: Some(root.path().join("velodex.log")),
                    ..LogConfig::default()
                },
                ..Config::default()
            },
            &backup,
            &mut Vec::new(),
        )
        .unwrap();

        assert!(
            std::fs::read_to_string(backup.join("config.toml"))
                .unwrap()
                .contains(expected)
        );
    }
}
