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
            source: None,
            last_modified: None,
            body: b"not json".to_vec(),
            ..cache_record(b"not json")
        },
    )
    .unwrap();
    meta.put_file_url("bad", "https://files.example/pkg.whl", "pypi")
        .unwrap();
    meta.put_metadata("bad", "also-bad").unwrap();
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
    meta.put_driver_value("pypi\u{0}o\u{0}//", b"bad").unwrap();
    drop(meta);
    let mut out = Vec::new();
    app::cache(&config, &fsck_command(), &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    for expected in [
        "metadata\tpypi\tindex\tpypi/bad\tinvalid project detail\n",
        "metadata\tpypi\tfile-url\tbad\tinvalid record\n",
        "metadata\tpypi\tpep658\tbad\tinvalid record\n",
        "metadata\tpypi\tproject\t/\tinvalid record\n",
        "metadata\tpypi\tupload\thosted/pkg/bad.whl\tinvalid record\n",
        "metadata\tpypi\tupload\t//\tinvalid key\n",
        "metadata\tpypi\tupload\thosted/pkg/pkg-1.0.whl\tmissing blob ",
        "metadata\tpypi\toverride\t//\tinvalid key\n",
        "problems\t10\n",
    ] {
        assert!(text.contains(expected), "{text}");
    }
    // The derived rows the empty-named project and upload above left behind. The write path maintains a
    // count and an order row for whatever index name it is handed, and no configured index is named "".
    for expected in [
        format!(
            "metadata\tpypi\tsummary-count\t{:?}\tno cached or hosted index owns this row\n",
            "pypi\u{0}k\u{0}"
        ),
        format!(
            "metadata\tpypi\tsummary-order\t{:?}\tno cached or hosted index owns this row\n",
            "pypi\u{0}w\u{0}\u{0}100000000000000000000/pkg-1.0.whl\u{0}\u{0}"
        ),
    ] {
        assert!(text.contains(&expected), "{text}");
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
        "metadata\tpypi\tupload\thosted/pkg/pkg-1.0.whl\tmissing blob {}",
        digest.as_str()
    )));
    assert!(text.contains(&format!(
        "metadata\tpypi\tupload\thosted/pkg/pkg-1.0.whl\tmissing blob {}",
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
    meta.set_override(
        true,
        "hosted",
        "pkg",
        "pkg-1.0.whl",
        peryx_ecosystem_pypi::store::OverrideMutation::Hidden(true),
        0,
    )
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
            .contains("metadata\tpypi\tindex\tpypi/corrupt\t")
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
