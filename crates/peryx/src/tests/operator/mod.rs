mod backup_tests;
mod import_tests;
mod integration_tests;
#[cfg(unix)]
mod permission_tests;
mod restore_tests;
mod verify_tests;
mod writer_tests;

use std::collections::BTreeMap;

use peryx_ecosystem_registry::pypi::store::PypiStore as _;
use peryx_ecosystem_registry::pypi::upload::Uploaded;
use peryx_ecosystem_registry::pypi::{CoreMetadata, File, Provenance, Yanked, to_json};
use peryx_storage::blob::{BlobStore, Digest};
use peryx_storage::meta::MetaStore;

use crate::config::Config;
use crate::operator;

/// A freshly created, valid backup: the source data dir, the temp root holding it, the source
/// config, the backup path, and the content and metadata blob digests.
pub(super) fn valid_backup() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    Config,
    std::path::PathBuf,
    Digest,
    Digest,
) {
    let (source, config, content_digest, metadata_digest) = backup_fixture();
    let root = tempfile::tempdir().unwrap();
    let backup = root.path().join("backup");
    operator::backup_create(&config, &backup, &mut Vec::new()).unwrap();
    (source, root, config, backup, content_digest, metadata_digest)
}

/// A data directory whose metadata store is claimed by `identity` and advanced by `mutations` distinct
/// project puts, so a test controls both the node identity and the control-plane serial.
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

/// A valid backup of a data directory claimed by `identity` and advanced by `mutations` puts. Returns
/// the temp root holding the source and the backup, and the backup path.
pub(super) fn identified_backup(identity: &str, mutations: usize) -> (tempfile::TempDir, std::path::PathBuf) {
    let holder = tempfile::tempdir().unwrap();
    let data_dir = holder.path().join("data");
    std::fs::create_dir(&data_dir).unwrap();
    let meta = MetaStore::open(data_dir.join("peryx.redb")).unwrap();
    meta.claim_writer_identity(identity).unwrap();
    for _ in 0..mutations {
        meta.next_serial().unwrap();
    }
    drop(meta);
    let config = Config {
        data_dir,
        writer_identity: Some(identity.to_owned()),
        ..Config::default()
    };
    let backup = holder.path().join("backup");
    operator::backup_create(&config, &backup, &mut Vec::new()).unwrap();
    (holder, backup)
}

pub(super) fn backup_fixture() -> (tempfile::TempDir, Config, Digest, Digest) {
    let dir = tempfile::tempdir().unwrap();
    let data_dir = dir.path().join("data");
    std::fs::create_dir(&data_dir).unwrap();
    let blobs = BlobStore::new(data_dir.join("blobs"));
    let content_digest = blobs.write(b"wheel bytes").unwrap();
    let metadata_digest = blobs.write(b"metadata bytes").unwrap();
    let meta = MetaStore::open(data_dir.join("peryx.redb")).unwrap();
    meta.put_upload(
        "hosted",
        "flask",
        "Flask-1.0-py3-none-any.whl",
        &uploaded_record_json(&content_digest, &metadata_digest),
    )
    .unwrap();
    meta.put_metadata(content_digest.as_str(), "uploaded", metadata_digest.as_str(), "hosted")
        .unwrap();
    meta.put_project("hosted", "flask", "Flask").unwrap();
    drop(meta);
    (
        dir,
        Config {
            data_dir,
            ..Config::default()
        },
        content_digest,
        metadata_digest,
    )
}

fn uploaded_record_json(content_digest: &Digest, metadata_digest: &Digest) -> Vec<u8> {
    to_json(&Uploaded {
        version: "1.0".to_owned(),
        file: File {
            filename: "Flask-1.0-py3-none-any.whl".to_owned(),
            url: format!(
                "/root/pypi/files/{}/Flask-1.0-py3-none-any.whl",
                content_digest.as_str()
            ),
            hashes: BTreeMap::from([("sha256".to_owned(), content_digest.as_str().to_owned())]),
            requires_python: None,
            size: Some(11),
            upload_time: Some("1970-01-01T00:00:00Z".to_owned()),
            yanked: Yanked::No,
            core_metadata: CoreMetadata::Hashes(BTreeMap::from([(
                "sha256".to_owned(),
                metadata_digest.as_str().to_owned(),
            )])),
            dist_info_metadata: CoreMetadata::Absent,
            gpg_sig: None,
            provenance: Provenance::Absent,
        },
        trashed: None,
    })
    .into_bytes()
}

pub(super) fn blob_relpath(digest: &Digest) -> String {
    let hex = digest.as_str();
    format!("blobs/sha256/{}/{}/{}", &hex[0..2], &hex[2..4], hex)
}

/// A report sink that fails the write carrying `needle` and records everything written up to it, so a
/// test can drive one report line's `?` error path and assert which line was being emitted.
#[derive(Default)]
pub(super) struct FailOnLine {
    pub(super) needle: &'static str,
    pub(super) seen: String,
}

impl std::io::Write for FailOnLine {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.seen.push_str(&String::from_utf8_lossy(buf));
        if self.seen.contains(self.needle) {
            return Err(std::io::Error::other("report sink closed"));
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
