mod import_tests;
mod integration_tests;
#[cfg(unix)]
mod permission_tests;
mod restore_tests;
mod writer_tests;

use std::collections::BTreeMap;
use std::io::Cursor;

use peryx_ecosystem_pypi::store::PypiStore as _;
use peryx_ecosystem_pypi::upload::Uploaded;
use peryx_ecosystem_pypi::{CoreMetadata, File, Provenance, Yanked, to_json};
use peryx_storage::blob::{BlobStore, Digest};
use peryx_storage::meta::MetaStore;

use crate::config::Config;
use crate::operator;

fn valid_backup() -> (
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

fn claimed_data_dir(identity: &str, mutations: usize) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    meta.claim_writer_identity(identity).unwrap();
    for _ in 0..mutations {
        meta.next_serial().unwrap();
    }
    drop(meta);
    dir
}

fn identified_backup(identity: &str, mutations: usize) -> (tempfile::TempDir, std::path::PathBuf) {
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

fn backup_fixture() -> (tempfile::TempDir, Config, Digest, Digest) {
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
    meta.put_metadata(content_digest.as_str(), metadata_digest.as_str())
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

fn blob_relpath(digest: &Digest) -> String {
    let hex = digest.as_str();
    format!("blobs/sha256/{}/{}/{}", &hex[0..2], &hex[2..4], hex)
}

fn bounded_through_line(output: &[u8], needle: &str) -> Cursor<Box<[u8]>> {
    let start = output
        .windows(needle.len())
        .position(|window| window == needle.as_bytes())
        .expect("successful output contains the failure boundary");
    Cursor::new(vec![0; start + output[start..].iter().position(|byte| *byte == b'\n').unwrap()].into_boxed_slice())
}
