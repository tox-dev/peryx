use std::collections::BTreeMap;

use peryx_ecosystem_pypi::upload::Uploaded;
use peryx_ecosystem_pypi::{CoreMetadata, File, Provenance, Yanked};
use peryx_storage::blob::{BlobStore, Digest};
use peryx_storage::meta::{CachedIndex, MetaStore};

use crate::cli::RuntimeArgs;
use crate::config::Config;

mod cache_tests;
mod fsck_tests;
mod indexes_tests;
mod policy_tests;
mod purge_tests;

pub(super) struct FailImmediately;

impl std::io::Write for FailImmediately {
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::other("write failed"))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct FailOnText {
    pub(super) needle: &'static str,
    pub(super) seen: String,
}

impl std::io::Write for FailOnText {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.seen.push_str(&String::from_utf8_lossy(buf));
        if self.seen.contains(self.needle) {
            return Err(std::io::Error::other("write failed"));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) fn cache_record(body: &[u8]) -> CachedIndex {
    CachedIndex {
        etag: None,
        last_serial: None,
        fetched_at_unix: 0,
        content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
        fresh_secs: Some(1),
        body: body.to_vec(),
    }
}

pub(super) fn cache_fixture() -> (tempfile::TempDir, Config, Digest) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let digest = BlobStore::new(dir.path().join("blobs")).write(b"payload").unwrap();
    let metadata_digest = Digest::of(b"metadata");
    meta.put_index(
        "pypi/flask",
        &CachedIndex {
            body: format!(
                r#"{{"meta":{{"api-version":"1.1"}},"name":"flask","versions":["1.0"],"files":[{{"filename":"flask-1.0.whl","url":"https://files.example/flask.whl","hashes":{{"sha256":"{}"}},"core-metadata":{{"sha256":"{}"}},"yanked":false}}]}}"#,
                digest.as_str(),
                metadata_digest.as_str()
            )
            .into_bytes(),
            ..cache_record(b"")
        },
    )
    .unwrap();
    meta.put_project("pypi", "flask", "Flask").unwrap();
    meta.put_file_url(digest.as_str(), "https://files.example/flask.whl", "pypi")
        .unwrap();
    meta.put_metadata(
        digest.as_str(),
        "https://files.example/flask.whl.metadata",
        metadata_digest.as_str(),
        "pypi",
    )
    .unwrap();
    let config = config_at(&dir);
    (dir, config, digest)
}

pub(super) fn uploaded_record_json(digest: &Digest) -> Vec<u8> {
    let mut hashes = BTreeMap::new();
    hashes.insert("sha256".to_owned(), digest.as_str().to_owned());
    serde_json::to_vec(&Uploaded {
        version: "1.0".to_owned(),
        file: File {
            filename: "pkg-1.0.whl".to_owned(),
            url: format!("http://localhost/files/{}/pkg-1.0.whl", digest.as_str()),
            hashes,
            requires_python: None,
            size: Some(3),
            upload_time: None,
            yanked: Yanked::No,
            core_metadata: CoreMetadata::Absent,
            dist_info_metadata: CoreMetadata::Absent,
            gpg_sig: None,
            provenance: Provenance::Absent,
        },
    })
    .unwrap()
}

pub(super) fn write_invalid_blob_path(root: &std::path::Path) {
    let path = root.join("blobs/sha256/aa/bb/not-a-digest");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, b"x").unwrap();
}

pub(super) fn raw_insert_bytes(path: &std::path::Path, table: &'static str, key: &str, value: &[u8]) {
    let db = redb::Database::open(path).unwrap();
    let table = redb::TableDefinition::<&str, &[u8]>::new(table);
    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(table).unwrap();
        table.insert(key, value).unwrap();
    }
    txn.commit().unwrap();
}

pub(super) fn config_at(dir: &tempfile::TempDir) -> Config {
    Config {
        data_dir: dir.path().to_path_buf(),
        ..Config::default()
    }
}

pub(super) fn store_and_config() -> (tempfile::TempDir, MetaStore, Config) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let config = config_at(&dir);
    (dir, meta, config)
}

pub(super) fn runtime_args() -> RuntimeArgs {
    RuntimeArgs {
        config: None,
        host: None,
        port: None,
        data_dir: None,
        offline: false,
        log_level: None,
        verbose: 0,
        log_format: None,
        log_sink: None,
        log_file: None,
    }
}
