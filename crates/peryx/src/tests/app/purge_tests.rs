use super::*;
use crate::app;
use crate::cli::{CacheCommand, CachePurgeCommand, CachePurgeOrphanedBlobsArgs, CachePurgeProjectArgs};

#[test]
fn test_cache_purge_project_dry_run_keeps_records() {
    let (_dir, config, digest) = cache_fixture();
    let mut out = Vec::new();
    app::cache(&config, &purge_project_command(false), &mut out).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "action\ttarget\tindex\tproject\tindex_pages\tproject_records\tfile_url_records\tmetadata_records\n\
dry-run\tproject\tpypi\tflask\t1\t1\t1\t1\n"
    );
    let meta = MetaStore::open_existing(config.data_dir.join("peryx.redb")).unwrap();
    assert!(meta.get_index("pypi/flask").unwrap().is_some());
    assert!(meta.get_file_url(digest.as_str()).unwrap().is_some());
}

#[test]
fn test_cache_purge_project_missing_target_is_empty() {
    let (_dir, config, _digest) = cache_fixture();
    let mut out = Vec::new();
    app::cache(
        &config,
        &CacheCommand::Purge(CachePurgeCommand::Project(CachePurgeProjectArgs {
            runtime: runtime_args(),
            index: "pypi".to_owned(),
            project: "missing".to_owned(),
            yes: false,
        })),
        &mut out,
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "action\ttarget\tindex\tproject\tindex_pages\tproject_records\tfile_url_records\tmetadata_records\n\
dry-run\tproject\tpypi\tmissing\t0\t0\t0\t0\n"
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
                r#"{{"meta":{{"api-version":"1.1"}},"name":"other","versions":["1.0"],"files":[{{"filename":"other-1.0.whl","url":"https://files.example/other.whl","hashes":{{"sha256":"{}"}},"core-metadata":false,"yanked":false}}]}}"#,
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
    app::cache(&config, &purge_project_command(false), &mut out).unwrap();
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("dry-run\tproject\tpypi\tflask\t1\t1\t0\t0\n")
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
    let err = app::cache(&config, &purge_project_command(false), &mut out).unwrap_err();
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
    let err = app::cache(&config, &purge_project_command(false), &mut out).unwrap_err();
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
    let err = app::cache(&config, &purge_project_command(false), &mut out).unwrap_err();
    assert!(err.to_string().contains("invalid upload record"), "{err}");
}

#[test]
fn test_cache_purge_project_rejects_invalid_cached_file_digest() {
    let (_dir, meta, config) = store_and_config();
    meta.put_index(
        "pypi/flask",
        &CachedIndex {
            body: br#"{"meta":{"api-version":"1.1"},"name":"flask","versions":["1.0"],"files":[{"filename":"flask-1.0.whl","url":"https://files.example/flask.whl","hashes":{"sha256":"bad"},"core-metadata":false,"yanked":false}]}"#.to_vec(),
            ..cache_record(b"")
        },
    )
    .unwrap();
    drop(meta);
    let mut out = Vec::new();
    let err = app::cache(&config, &purge_project_command(false), &mut out).unwrap_err();
    assert!(
        err.chain()
            .any(|cause| cause.to_string().contains("invalid sha256 digest"))
    );
}

#[test]
fn test_cache_purge_project_rejects_invalid_cached_metadata_digest() {
    let (_dir, meta, config) = store_and_config();
    let digest = Digest::of(b"wheel");
    meta.put_index(
        "pypi/flask",
        &CachedIndex {
            body: format!(
                r#"{{"meta":{{"api-version":"1.1"}},"name":"flask","versions":["1.0"],"files":[{{"filename":"flask-1.0.whl","url":"https://files.example/flask.whl","hashes":{{"sha256":"{}"}},"core-metadata":{{"sha256":"bad"}},"yanked":false}}]}}"#,
                digest.as_str()
            )
            .into_bytes(),
            ..cache_record(b"")
        },
    )
    .unwrap();
    drop(meta);
    let mut out = Vec::new();
    let err = app::cache(&config, &purge_project_command(false), &mut out).unwrap_err();
    assert!(
        err.chain()
            .any(|cause| cause.to_string().contains("invalid metadata digest"))
    );
}

#[test]
fn test_cache_purge_project_ignores_files_without_sha256() {
    let (_dir, meta, config) = store_and_config();
    meta.put_index(
        "pypi/flask",
        &CachedIndex {
            body: br#"{"meta":{"api-version":"1.1"},"name":"flask","versions":["1.0"],"files":[{"filename":"flask-1.0.whl","url":"https://files.example/flask.whl","hashes":{},"core-metadata":false,"yanked":false}]}"#.to_vec(),
            ..cache_record(b"")
        },
    )
    .unwrap();
    drop(meta);
    let mut out = Vec::new();
    app::cache(&config, &purge_project_command(false), &mut out).unwrap();
    assert!(
        String::from_utf8(out)
            .unwrap()
            .contains("dry-run\tproject\tpypi\tflask\t1\t0\t0\t0\n")
    );
}

#[test]
fn test_cache_purge_project_reports_write_errors() {
    let (_dir, config, _digest) = cache_fixture();
    let mut out = FailOnText {
        needle: "dry-run",
        seen: String::new(),
    };
    let err = app::cache(&config, &purge_project_command(false), &mut out).unwrap_err();
    assert!(err.to_string().contains("write failed"));
}

#[test]
fn test_cache_purge_project_yes_removes_metadata_records() {
    let (_dir, config, digest) = cache_fixture();
    let mut out = Vec::new();
    app::cache(&config, &purge_project_command(true), &mut out).unwrap();
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "action\ttarget\tindex\tproject\tindex_pages\tproject_records\tfile_url_records\tmetadata_records\n\
removed\tproject\tpypi\tflask\t1\t1\t1\t1\n"
    );
    let meta = MetaStore::open_existing(config.data_dir.join("peryx.redb")).unwrap();
    assert!(meta.get_index("pypi/flask").unwrap().is_none());
    assert!(meta.get_file_url(digest.as_str()).unwrap().is_none());
    assert!(meta.get_metadata(digest.as_str()).unwrap().is_none());
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
            meta.put_metadata(&wheel, "https://files.example/pkg.whl.metadata", &metadata, "pypi")
                .unwrap();
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

#[test]
fn test_cache_purge_orphaned_blobs_reports_write_errors() {
    let (_dir, config, _digest) = cache_fixture();
    let blobs = BlobStore::new(config.data_dir.join("blobs"));
    blobs.write(b"orphan").unwrap();
    for needle in ["orphaned-blob", "summary"] {
        let mut out = FailOnText {
            needle,
            seen: String::new(),
        };
        let err = app::cache(&config, &purge_orphaned_blobs_command(false), &mut out).unwrap_err();
        assert!(err.to_string().contains("scan orphaned blob files") || err.to_string().contains("write failed"));
    }
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

fn purge_project_command(yes: bool) -> CacheCommand {
    CacheCommand::Purge(CachePurgeCommand::Project(CachePurgeProjectArgs {
        runtime: runtime_args(),
        index: "pypi".to_owned(),
        project: "Flask".to_owned(),
        yes,
    }))
}

fn purge_orphaned_blobs_command(yes: bool) -> CacheCommand {
    CacheCommand::Purge(CachePurgeCommand::OrphanedBlobs(CachePurgeOrphanedBlobsArgs {
        runtime: runtime_args(),
        yes,
    }))
}
