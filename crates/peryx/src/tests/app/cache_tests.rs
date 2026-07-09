use rstest::rstest;

use super::*;
use crate::app;
use crate::cli::{CacheCommand, CacheListArgs, CacheRuntimeArgs};

#[test]
fn test_cache_list_reports_index_pages_and_blobs() {
    let (_dir, config, digest) = cache_fixture();
    let mut out = Vec::new();
    app::cache(&config, &cache_list_command(), &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains(CACHE_LIST_HEADER));
    assert!(text.contains("index\tpypi\tflask\t\t"));
    assert!(text.contains(&format!("blob\t\t\t{}\t-\t-\t-\t7\t", digest.as_str())));
}

#[test]
fn test_cache_list_reports_write_errors() {
    let (_dir, config, _digest) = cache_fixture();
    let err = app::cache(&config, &cache_list_command(), &mut FailImmediately).unwrap_err();
    assert!(err.to_string().contains("write failed"));

    let mut out = FailOnText {
        needle: "index\tpypi\tflask",
        seen: String::new(),
    };
    let err = app::cache(&config, &cache_list_command(), &mut out).unwrap_err();
    assert!(err.to_string().contains("scan cached index pages"));
}

#[test]
fn test_cache_list_handles_unknown_index_keys() {
    let (_dir, meta, config) = store_and_config();
    meta.put_index("loose", &cache_record(br#"{"files":[]}"#)).unwrap();
    meta.put_index("other/flask", &cache_record(br#"{"files":[]}"#))
        .unwrap();
    meta.put_index(
        "pypi/no-ttl",
        &CachedIndex {
            fresh_secs: None,
            ..cache_record(br#"{"files":[]}"#)
        },
    )
    .unwrap();
    drop(meta);
    let mut out = Vec::new();
    app::cache(&config, &cache_list_command(), &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("index\tloose\t\t\t"));
    assert!(text.contains("index\tother\tflask\t\t"));
    assert!(text.contains("index\tpypi\tno-ttl\t\t"));
    assert!(text.contains("\t-\ttrue\t"));
}

#[test]
fn test_cache_missing_store_errors_with_path() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config {
        data_dir: dir.path().join("missing"),
        ..Config::default()
    };
    let mut out = Vec::new();
    let err = app::cache(&config, &size_command(), &mut out).unwrap_err();
    assert!(err.to_string().contains("open metadata store"));
}

#[rstest]
#[case::unmatched_index(Some("missing"), None, None, None)]
#[case::unmatched_project(None, Some("missing"), None, None)]
#[case::size_over_limit(None, None, None, Some(1024))]
#[case::unmatched_digest(None, None, Some("0".repeat(64)), None)]
fn test_cache_list_filters_skip_nonmatching_rows(
    #[case] index: Option<&str>,
    #[case] project: Option<&str>,
    #[case] digest: Option<String>,
    #[case] min_size_bytes: Option<u64>,
) {
    let (_dir, config, _digest) = cache_fixture();
    let mut out = Vec::new();
    app::cache(
        &config,
        &CacheCommand::List(CacheListArgs {
            runtime: runtime_args(),
            index: index.map(str::to_owned),
            project: project.map(str::to_owned),
            digest,
            stale: false,
            min_age_secs: None,
            min_size_bytes,
        }),
        &mut out,
    )
    .unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), CACHE_LIST_HEADER);
}

#[test]
fn test_cache_list_min_age_filter_skips_future_pages() {
    let (_dir, meta, config) = store_and_config();
    meta.put_index(
        "pypi/future",
        &CachedIndex {
            fetched_at_unix: i64::MAX,
            ..cache_record(br#"{"files":[]}"#)
        },
    )
    .unwrap();
    drop(meta);
    let mut out = Vec::new();
    app::cache(
        &config,
        &CacheCommand::List(CacheListArgs {
            runtime: runtime_args(),
            index: None,
            project: None,
            digest: None,
            stale: false,
            min_age_secs: Some(1),
            min_size_bytes: None,
        }),
        &mut out,
    )
    .unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), CACHE_LIST_HEADER);
}

#[test]
fn test_cache_list_skips_invalid_blob_paths() {
    let dir = tempfile::tempdir().unwrap();
    MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    write_invalid_blob_path(dir.path());
    let config = config_at(&dir);
    let mut out = Vec::new();
    app::cache(&config, &cache_list_command(), &mut out).unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), CACHE_LIST_HEADER);
}

#[test]
fn test_cache_size_reports_counts_and_stale_pages() {
    let (_dir, config, _digest) = cache_fixture();
    let mut out = Vec::new();
    app::cache(&config, &size_command(), &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("index_pages\t1\n"));
    assert!(text.contains("stale_index_pages\t1\n"));
    assert!(text.contains("blob_files\t1\n"));
    assert!(text.contains("blob_bytes\t7\n"));
    assert!(text.contains("file_url_records\t1\n"));
    assert!(text.contains("metadata_records\t1\n"));
    assert!(text.contains("project_records\t1\n"));
}

#[test]
fn test_cache_size_counts_invalid_blob_paths() {
    let dir = tempfile::tempdir().unwrap();
    MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    write_invalid_blob_path(dir.path());
    let config = config_at(&dir);
    let mut out = Vec::new();
    app::cache(&config, &size_command(), &mut out).unwrap();
    assert!(String::from_utf8(out).unwrap().contains("invalid_blob_paths\t1\n"));
}

#[test]
fn test_cache_size_counts_uploads_and_overrides() {
    let (_dir, meta, config) = store_and_config();
    meta.put_upload(
        "hosted",
        "pkg",
        "pkg-1.0.whl",
        &uploaded_record_json(&Digest::of(b"pkg")),
    )
    .unwrap();
    meta.put_override("hosted", "pkg", "pkg-1.0.whl", "hidden").unwrap();
    drop(meta);
    let mut out = Vec::new();
    app::cache(&config, &size_command(), &mut out).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("upload_records\t1\n"));
    assert!(text.contains("override_records\t1\n"));
}

const CACHE_LIST_HEADER: &str = "kind\tindex\tproject\tdigest\tage_secs\tfresh_secs\tstale\tsize_bytes\tkey\n";

fn cache_list_command() -> CacheCommand {
    CacheCommand::List(CacheListArgs {
        runtime: runtime_args(),
        index: None,
        project: None,
        digest: None,
        stale: false,
        min_age_secs: None,
        min_size_bytes: None,
    })
}

fn size_command() -> CacheCommand {
    CacheCommand::Size(CacheRuntimeArgs {
        runtime: runtime_args(),
    })
}
