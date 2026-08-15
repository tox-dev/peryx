use std::io::Cursor;
use std::path::{Path, PathBuf};

use peryx_storage::blob::{BlobStore, Digest};
use peryx_storage::meta::MetaStore;

use crate::config::{BlobStorageConfig, Config, S3StorageConfig};
use crate::operator;
use crate::tests::support::{plugins, plugins_with_blob_references};

pub(super) struct BackupFixture {
    pub(super) _source: tempfile::TempDir,
    pub(super) root: tempfile::TempDir,
    pub(super) config: Config,
    pub(super) backup: PathBuf,
    pub(super) content_digest: Digest,
    pub(super) metadata_digest: Digest,
}

pub(super) fn valid_backup() -> BackupFixture {
    let (source, config, content_digest, metadata_digest) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup");
    backup_create_with_references(&config, &backup, &mut Vec::new()).unwrap();
    BackupFixture {
        _source: source,
        root,
        config,
        backup,
        content_digest,
        metadata_digest,
    }
}

pub(super) fn backup_fixture() -> (tempfile::TempDir, Config, Digest, Digest) {
    let source = tempfile::tempdir().unwrap();
    let data_dir = source.path().join("data");
    std::fs::create_dir(&data_dir).unwrap();
    let blobs = BlobStore::new(data_dir.join("blobs"));
    let content_digest = blobs.write(b"artifact bytes").unwrap();
    let metadata_digest = blobs.write(b"metadata bytes").unwrap();
    drop(MetaStore::open(data_dir.join("peryx.redb")).unwrap());
    let plugins = plugins();
    (
        source,
        Config {
            data_dir,
            ..Config::with_plugins(&plugins)
        },
        content_digest,
        metadata_digest,
    )
}

pub(super) fn identified_backup(identity: &str, mutations: usize) -> (tempfile::TempDir, PathBuf) {
    let holder = tempfile::tempdir().unwrap();
    let data_dir = holder.path().join("data");
    std::fs::create_dir(&data_dir).unwrap();
    let blobs = BlobStore::new(data_dir.join("blobs"));
    blobs.write(b"artifact bytes").unwrap();
    blobs.write(b"metadata bytes").unwrap();
    let meta = MetaStore::open(data_dir.join("peryx.redb")).unwrap();
    meta.claim_writer_identity(identity).unwrap();
    for _ in 0..mutations {
        meta.next_serial().unwrap();
    }
    drop(meta);
    let backup = holder.path().join("backup");
    backup_create_with_references(
        &Config {
            data_dir,
            writer_identity: Some(identity.to_owned()),
            ..Config::with_plugins(&plugins())
        },
        &backup,
        &mut Vec::new(),
    )
    .unwrap();
    (holder, backup)
}

pub(super) fn backup_create(config: &Config, path: &Path, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
    operator::backup_create_with_plugins(config, &plugins(), path, out)
}

pub(super) fn backup_create_with_references(
    config: &Config,
    path: &Path,
    out: &mut dyn std::io::Write,
) -> anyhow::Result<()> {
    operator::backup_create_with_plugins(config, &plugins_with_blob_references(), path, out)
}

pub(super) fn backup_verify(path: &Path, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
    operator::backup_verify_with_plugins(path, &plugins_with_blob_references(), out)
}

pub(super) fn restore(backup: &Path, data_dir: &Path, force: bool, out: &mut dyn std::io::Write) -> anyhow::Result<()> {
    operator::restore_with_plugins(backup, data_dir, force, &plugins_with_blob_references(), out)
}

pub(super) fn claimed_data_dir(identity: &str, mutations: usize) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    meta.claim_writer_identity(identity).unwrap();
    for _ in 0..mutations {
        meta.next_serial().unwrap();
    }
    drop(meta);
    dir
}

pub(super) fn blob_relpath(digest: &Digest) -> String {
    let hex = digest.as_str();
    format!("blobs/sha256/{}/{}/{}", &hex[0..2], &hex[2..4], hex)
}

pub(super) fn mutate_manifest(backup: &Path, mutate: impl FnOnce(&mut serde_json::Value)) {
    let path = backup.join("manifest.json");
    let mut manifest = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    mutate(&mut manifest);
    std::fs::write(path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
}

pub(super) fn resign_file(backup: &Path, key: &str, relative: &str) {
    let bytes = std::fs::read(backup.join(relative)).unwrap();
    mutate_manifest(backup, |manifest| {
        manifest[key]["sha256"] = serde_json::json!(Digest::of(&bytes).as_str());
        manifest[key]["size_bytes"] = serde_json::json!(bytes.len());
    });
}

pub(super) fn bounded_before(output: &[u8], needle: &str) -> Cursor<Box<[u8]>> {
    Cursor::new(
        vec![
            0;
            output
                .windows(needle.len())
                .position(|window| window == needle.as_bytes())
                .unwrap()
        ]
        .into_boxed_slice(),
    )
}

pub(super) fn s3_blob_config() -> BlobStorageConfig {
    BlobStorageConfig::S3(S3StorageConfig {
        endpoint: "https://s3.example.com".to_owned(),
        bucket: "cache".to_owned(),
        prefix: String::new(),
        region: "us-east-1".to_owned(),
        path_style: false,
        request_timeout: std::time::Duration::from_secs(30),
        max_retries: 3,
        multipart_threshold: 16 << 20,
        part_size: 16 << 20,
        upload_concurrency: 4,
        conditional_writes: true,
        checksum_writes: true,
    })
}
