use super::*;
use crate::app;
use crate::cli::{CacheCommand, CacheRuntimeArgs};

#[test]
fn test_cache_fsck_reports_ok_for_valid_store() {
    let (_dir, config, _digest) = cache_fixture();
    let mut out = Vec::new();
    app::cache(&config, &fsck_command(), &mut out).unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "ok\n");
}

#[test]
fn test_cache_fsck_reports_metadata_problems() {
    let (_dir, meta, config) = store_and_config();
    meta.put_index(
        "pypi/bad",
        &CachedIndex {
            body: b"not json".to_vec(),
            ..cache_record(b"not json")
        },
    )
    .unwrap();
    meta.put_file_url("bad", "https://files.example/pkg.whl", "pypi")
        .unwrap();
    meta.put_metadata("bad", "https://files.example/pkg.whl.metadata", "also-bad", "pypi")
        .unwrap();
    meta.put_project("", "", "").unwrap();
    meta.put_upload("hosted", "pkg", "bad.whl", b"not json").unwrap();
    meta.put_upload("", "", "", &uploaded_record_json(&Digest::of(b"missing")))
        .unwrap();
    meta.put_upload(
        "hosted",
        "pkg",
        "pkg-1.0.whl",
        &uploaded_record_json(&Digest::of(b"missing")),
    )
    .unwrap();
    meta.put_override(true, "", "", "", "bad", 0).unwrap();
    drop(meta);
    let mut out = Vec::new();
    app::cache(&config, &fsck_command(), &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    for expected in [
        "metadata\tindex\tpypi/bad\tinvalid project detail\n",
        "metadata\tfile-url\tbad\tinvalid record\n",
        "metadata\tpep658\tbad\tinvalid record\n",
        "metadata\tproject\t/\tinvalid record\n",
        "metadata\tupload\thosted/pkg/bad.whl\tinvalid record\n",
        "metadata\tupload\t//\tinvalid key\n",
        "metadata\tupload\thosted/pkg/pkg-1.0.whl\tmissing blob ",
        "metadata\toverride\t//\tinvalid record\n",
        "problems\t8\n",
    ] {
        assert!(text.contains(expected), "{text}");
    }
}

#[test]
fn test_cache_fsck_reports_missing_metadata_blob() {
    let (_dir, meta, config) = store_and_config();
    let digest = Digest::of(b"wheel");
    let metadata_digest = Digest::of(b"metadata");
    meta.put_upload(
        "hosted",
        "pkg",
        "pkg-1.0.whl",
        &uploaded_record_json_with_metadata(&digest, &metadata_digest),
    )
    .unwrap();
    drop(meta);
    let mut out = Vec::new();
    app::cache(&config, &fsck_command(), &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains(&format!(
        "metadata\tupload\thosted/pkg/pkg-1.0.whl\tmissing blob {}",
        digest.as_str()
    )));
    assert!(text.contains(&format!(
        "metadata\tupload\thosted/pkg/pkg-1.0.whl\tmissing blob {}",
        metadata_digest.as_str()
    )));
}

#[test]
fn test_cache_fsck_accepts_valid_upload_and_override() {
    let (dir, meta, config) = store_and_config();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let digest = blobs.write(b"pkg").unwrap();
    meta.put_upload("hosted", "pkg", "pkg-1.0.whl", &uploaded_record_json(&digest))
        .unwrap();
    meta.put_override(true, "hosted", "pkg", "pkg-1.0.whl", "hidden", 0)
        .unwrap();
    drop(meta);
    let mut out = Vec::new();
    app::cache(&config, &fsck_command(), &mut out).unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), "ok\n");
}

#[test]
fn test_cache_fsck_reports_corrupt_index_record() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("peryx.redb");
    MetaStore::open(&db_path).unwrap();
    raw_insert_bytes(&db_path, "driver_kv", "pypi\u{0}i\u{0}pypi/corrupt", b"not json");
    let config = config_at(&dir);
    let mut out = Vec::new();
    app::cache(&config, &fsck_command(), &mut out).unwrap();
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("metadata\tindex\tpypi/corrupt\t")
    );
}

fn uploaded_record_json_with_metadata(digest: &Digest, metadata_digest: &Digest) -> Vec<u8> {
    let mut metadata_hashes = BTreeMap::new();
    metadata_hashes.insert("sha256".to_owned(), metadata_digest.as_str().to_owned());
    let mut upload: Uploaded = serde_json::from_slice(&uploaded_record_json(digest)).unwrap();
    upload.file.core_metadata = CoreMetadata::Hashes(metadata_hashes);
    serde_json::to_vec(&upload).unwrap()
}

const fn fsck_command() -> CacheCommand {
    CacheCommand::Fsck(CacheRuntimeArgs {
        runtime: runtime_args(),
    })
}
