use super::*;
use crate::app;
use crate::cli::{CacheCommand, CachePurgeCommand, CachePurgeOrphanedBlobsArgs, CachePurgeResourceArgs};
use rstest::rstest;

#[test]
fn test_cache_purge_project_dry_run_keeps_records() {
    let (_dir, config, digest) = cache_fixture();
    let mut out = Vec::new();
    app::cache(&config, &purge_resource_command(false), &mut out).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "action\ttarget\tindex\tresource\tindex_pages\tproject_records\tfile_url_records\tmetadata_records\n\
 dry-run\tresource\tpypi\tflask\t1\t1\t1\t1\n"
    );
    let meta = MetaStore::open_existing(config.data_dir.join("peryx.redb")).unwrap();
    assert!(meta.get_index("pypi/flask").unwrap().is_some());
    assert!(meta.get_file_url(digest.as_str()).unwrap().is_some());
}

#[test]
fn test_cache_purge_resource_missing_target_is_empty() {
    let (_dir, config, _digest) = cache_fixture();
    let mut out = Vec::new();
    app::cache(
        &config,
        &CacheCommand::Purge(CachePurgeCommand::Resource(CachePurgeResourceArgs {
            runtime: runtime_args(),
            index: "pypi".to_owned(),
            resource: "missing".to_owned(),
            yes: false,
        })),
        &mut out,
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "action\ttarget\tindex\tresource\tindex_pages\tproject_records\tfile_url_records\tmetadata_records\n\
 dry-run\tresource\tpypi\tmissing\t0\t0\t0\t0\n"
    );
}

#[test]
fn test_cache_purge_project_preserves_shared_and_uploaded_blobs() {
    let (_dir, config, digest) = cache_fixture();
    let meta = MetaStore::open_existing(config.data_dir.join("peryx.redb")).unwrap();
    meta.put_index(
        "pypi/other",
        &CachedIndex {
            body: format!(
                r#"{{"meta":{{"api-version":"1.1"}},"name":"other","versions":["1.0"],"files":[{{"filename":"other-1.0.whl","size":11,"url":"https://files.example/other.whl","hashes":{{"sha256":"{}"}},"core-metadata":false,"yanked":false}}]}}"#,
                digest.as_str()
            )
            .into_bytes(),
            ..cache_record(b"")
        },
    )
    .unwrap();
    meta.put_upload(
        "hosted",
        "pkg",
        "pkg-1.0.whl",
        &uploaded_record_json(&Digest::of(b"uploaded")),
    )
    .unwrap();
    drop(meta);
    let mut out = Vec::new();
    app::cache(&config, &purge_resource_command(false), &mut out).unwrap();
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("dry-run\tresource\tpypi\tflask\t1\t1\t0\t0\n")
    );
}

#[test]
fn test_cache_purge_project_reports_corrupt_target_record() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("peryx.redb");
    MetaStore::open(&db_path).unwrap();
    raw_insert_bytes(&db_path, "driver_kv", "pypi\u{0}i\u{0}pypi/flask", b"not json");
    let config = config_at(&dir);
    let mut out = Vec::new();
    let err = app::cache(&config, &purge_resource_command(false), &mut out).unwrap_err();
    assert!(
        err.chain()
            .any(|cause| cause.to_string().contains("read cached project pypi/flask"))
    );
}

#[test]
fn test_cache_purge_project_reports_corrupt_shared_record() {
    let (_dir, config, _digest) = cache_fixture();
    raw_insert_bytes(
        &config.data_dir.join("peryx.redb"),
        "driver_kv",
        "pypi\u{0}i\u{0}pypi/other",
        b"not json",
    );
    let mut out = Vec::new();
    let err = app::cache(&config, &purge_resource_command(false), &mut out).unwrap_err();
    assert!(err.to_string().contains("corrupt cached page"), "{err}");
}

#[test]
fn test_cache_purge_project_reports_corrupt_upload_record() {
    let (_dir, config, _digest) = cache_fixture();
    raw_insert_bytes(
        &config.data_dir.join("peryx.redb"),
        "driver_kv",
        "pypi\u{0}u\u{0}hosted/pkg/bad.whl",
        b"not json",
    );
    let mut out = Vec::new();
    let err = app::cache(&config, &purge_resource_command(false), &mut out).unwrap_err();
    assert!(err.to_string().contains("invalid upload record"), "{err}");
}

#[test]
fn test_cache_purge_project_ignores_files_without_sha256() {
    let (_dir, meta, config) = store_and_config();
    meta.put_index(
        "pypi/flask",
        &CachedIndex {
            body: br#"{"meta":{"api-version":"1.1"},"name":"flask","versions":["1.0"],"files":[{"filename":"flask-1.0.whl","size":11,"url":"https://files.example/flask.whl","hashes":{},"core-metadata":false,"yanked":false}]}"#.to_vec(),
            ..cache_record(b"")
        },
    )
    .unwrap();
    drop(meta);
    let mut out = Vec::new();
    app::cache(&config, &purge_resource_command(false), &mut out).unwrap();
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("dry-run\tresource\tpypi\tflask\t1\t0\t0\t0\n")
    );
}

#[test]
fn test_cache_purge_project_reports_write_errors() {
    let (_dir, config, _digest) = cache_fixture();
    let mut out = bounded_output(
        "action\ttarget\tindex\tresource\tindex_pages\tproject_records\tfile_url_records\tmetadata_records\n".len(),
    );
    let err = app::cache(&config, &purge_resource_command(false), &mut out).unwrap_err();
    assert!(err.to_string().contains("failed to write whole buffer"));
}

#[test]
fn test_cache_purge_project_yes_removes_metadata_records() {
    let (_dir, config, digest) = cache_fixture();
    let mut out = Vec::new();
    app::cache(&config, &purge_resource_command(true), &mut out).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "action\ttarget\tindex\tresource\tindex_pages\tproject_records\tfile_url_records\tmetadata_records\n\
 removed\tresource\tpypi\tflask\t1\t1\t1\t1\n"
    );
    let meta = MetaStore::open_existing(config.data_dir.join("peryx.redb")).unwrap();
    assert!(meta.get_index("pypi/flask").unwrap().is_none());
    assert!(meta.get_file_url(digest.as_str()).unwrap().is_none());
    assert!(meta.get_metadata_digest(digest.as_str()).unwrap().is_none());
    assert!(meta.list_projects("pypi").unwrap().is_empty());
}

#[test]
fn test_cache_purge_orphaned_blobs_rejects_invalid_references() {
    let (_dir, meta, config) = store_and_config();
    meta.put_file_url("bad", "https://files.example/pkg.whl", "pypi")
        .unwrap();
    drop(meta);
    let mut out = Vec::new();
    let err = app::cache(&config, &purge_orphaned_blobs_command(false), &mut out).unwrap_err();
    assert!(err.to_string().contains("invalid file URL record"), "{err}");
}

#[test]
fn test_cache_purge_orphaned_blobs_rejects_invalid_metadata_references() {
    let valid = Digest::of(b"valid");
    for (wheel, metadata, raw) in [
        ("bad".to_owned(), valid.as_str().to_owned(), None),
        (valid.as_str().to_owned(), "bad".to_owned(), None),
        (
            valid.as_str().to_owned(),
            valid.as_str().to_owned(),
            Some("missing-parts"),
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("peryx.redb");
        let meta = MetaStore::open(&db_path).unwrap();
        if let Some(raw) = raw {
            drop(meta);
            raw_insert_bytes(
                &db_path,
                "driver_kv",
                &format!("pypi\u{0}d\u{0}{wheel}"),
                raw.as_bytes(),
            );
        } else {
            meta.put_metadata(&wheel, &metadata).unwrap();
            drop(meta);
        }
        let config = config_at(&dir);
        let mut out = Vec::new();
        let err = app::cache(&config, &purge_orphaned_blobs_command(false), &mut out).unwrap_err();
        assert!(err.to_string().contains("PEP 658"), "{err}");
    }
}

#[test]
fn test_cache_purge_orphaned_blobs_rejects_invalid_upload_references() {
    let (_dir, meta, config) = store_and_config();
    meta.put_upload("hosted", "pkg", "bad.whl", b"not json").unwrap();
    drop(meta);
    let mut out = Vec::new();
    let err = app::cache(&config, &purge_orphaned_blobs_command(false), &mut out).unwrap_err();
    assert!(err.to_string().contains("invalid upload record"), "{err}");
}

#[test]
fn test_cache_purge_orphaned_blobs_keeps_referenced_upload_blobs() {
    let (dir, meta, config) = store_and_config();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let digest = blobs.write(b"pkg").unwrap();
    meta.put_upload("hosted", "pkg", "pkg-1.0.whl", &uploaded_record_json(&digest))
        .unwrap();
    drop(meta);
    let mut out = Vec::new();
    app::cache(&config, &purge_orphaned_blobs_command(false), &mut out).unwrap();
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("summary\tdry-run\torphaned-blobs\t0\t0\n")
    );
}

#[test]
fn test_cache_purge_orphaned_blobs_skips_invalid_blob_paths() {
    let dir = tempfile::tempdir().unwrap();
    MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    write_invalid_blob_path(dir.path());
    let config = config_at(&dir);
    let mut out = Vec::new();
    app::cache(&config, &purge_orphaned_blobs_command(false), &mut out).unwrap();
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("summary\tdry-run\torphaned-blobs\t0\t0\n")
    );
}

#[rstest]
#[case::row("orphaned-blob")]
#[case::summary("summary")]
fn test_cache_purge_orphaned_blobs_reports_write_errors(#[case] boundary: &str) {
    let (_dir, config, _digest) = cache_fixture();
    let blobs = BlobStore::new(config.data_dir.join("blobs"));
    blobs.write(b"orphan").unwrap();
    let mut complete = Vec::new();
    app::cache(&config, &purge_orphaned_blobs_command(false), &mut complete).unwrap();

    let err = app::cache(
        &config,
        &purge_orphaned_blobs_command(false),
        &mut bounded_before(&complete, boundary),
    )
    .unwrap_err();

    assert!(err.to_string().contains("failed to write whole buffer"), "{err}");
}

#[test]
fn test_cache_purge_orphaned_blobs_dry_run_keeps_blob() {
    let (_dir, config, _digest) = cache_fixture();
    let blobs = BlobStore::new(config.data_dir.join("blobs"));
    let orphan = blobs.write(b"orphan").unwrap();
    let mut out = Vec::new();
    app::cache(&config, &purge_orphaned_blobs_command(false), &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains(&format!("dry-run\torphaned-blob\t{}\t6\t", orphan.as_str())));
    assert!(text.contains("summary\tdry-run\torphaned-blobs\t1\t6\n"));
    assert!(blobs.exists(&orphan));
}

#[test]
fn test_cache_purge_orphaned_blobs_yes_removes_blob() {
    let (_dir, config, _digest) = cache_fixture();
    let blobs = BlobStore::new(config.data_dir.join("blobs"));
    let orphan = blobs.write(b"orphan").unwrap();
    let mut out = Vec::new();
    app::cache(&config, &purge_orphaned_blobs_command(true), &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains(&format!("removed\torphaned-blob\t{}\t6\t", orphan.as_str())));
    assert!(text.contains("summary\tremoved\torphaned-blobs\t1\t6\n"));
    assert!(!blobs.exists(&orphan));
}

#[rstest]
#[case::resource(purge_resource_command(false), "dry-run\tresource\tpypi\tflask\t1\t1\t1\t1\n")]
#[case::orphaned_blobs(purge_orphaned_blobs_command(false), "summary\tdry-run\torphaned-blobs\t0\t0\n")]
fn test_cache_purge_dry_run_reads_a_store_it_cannot_write(#[case] command: CacheCommand, #[case] expected: &str) {
    let (_dir, config, _digest) = cache_fixture();
    let path = config.data_dir.join("peryx.redb");
    set_writable(&path, false);

    let mut out = Vec::new();
    let purged = app::cache(&config, &command, &mut out);

    set_writable(&path, true);
    purged.unwrap();
    assert!(String::from_utf8(out).unwrap().contains(expected));
}

#[test]
fn test_cache_purge_confirmation_still_takes_the_writable_handle() {
    let (_dir, config, _digest) = cache_fixture();
    let path = config.data_dir.join("peryx.redb");
    set_writable(&path, false);

    let purged = app::cache(&config, &purge_resource_command(true), &mut Vec::new());

    set_writable(&path, true);
    let reported = format!("{:#}", purged.unwrap_err());
    assert!(reported.contains("open metadata store"), "{reported}");
}

/// `Permissions::set_readonly` is the one write bit every platform agrees on: it clears the mode's
/// write bits on unix and sets the read-only attribute on Windows, and either one stops redb taking
/// the read-write handle while leaving the read-only one open.
fn set_writable(path: &std::path::Path, writable: bool) {
    let mut permissions = std::fs::metadata(path).unwrap().permissions();
    permissions.set_readonly(!writable);
    std::fs::set_permissions(path, permissions).unwrap();
}

fn purge_resource_command(yes: bool) -> CacheCommand {
    CacheCommand::Purge(CachePurgeCommand::Resource(CachePurgeResourceArgs {
        runtime: runtime_args(),
        index: "pypi".to_owned(),
        resource: "Flask".to_owned(),
        yes,
    }))
}

const fn purge_orphaned_blobs_command(yes: bool) -> CacheCommand {
    CacheCommand::Purge(CachePurgeCommand::OrphanedBlobs(CachePurgeOrphanedBlobsArgs {
        runtime: runtime_args(),
        yes,
    }))
}
