use super::{MetaStore, UPLOAD_PREFIX};
use crate::store::PypiStore as _;

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, meta)
}

fn upload(meta: &MetaStore, index: &str, project: &str, filename: &str, version: &str, at: &str, size: u64) {
    let record =
        format!(r#"{{"version":"{version}","file":{{"filename":"{filename}","upload-time":"{at}","size":{size}}}}}"#);
    meta.put_upload(index, project, filename, record.as_bytes()).unwrap();
}

#[test]
fn test_summarize_indexes_counts_projects_and_orders_recent_uploads() {
    let (_dir, meta) = store();
    meta.put_project("hosted", "flask", "Flask").unwrap();
    meta.put_project("root/hosted", "django", "Django").unwrap();
    upload(
        &meta,
        "hosted",
        "flask",
        "flask-1.0.whl",
        "1.0",
        "2026-01-01T00:00:00Z",
        10,
    );
    upload(
        &meta,
        "root/hosted",
        "django",
        "django-4.0.whl",
        "4.0",
        "2026-02-01T00:00:00Z",
        20,
    );
    upload(
        &meta,
        "root/hosted",
        "django",
        "django-3.2.whl",
        "3.2",
        "2025-12-01T00:00:00Z",
        15,
    );
    // An upload on an index the caller did not ask about is ignored.
    upload(
        &meta,
        "foreign",
        "flask",
        "ignored.whl",
        "1.0",
        "2026-03-01T00:00:00Z",
        5,
    );

    let indexes = vec!["hosted".to_owned(), "root/hosted".to_owned()];
    let summary = meta.summarize_indexes(&indexes, 5).unwrap();

    assert_eq!(summary["hosted"].project_count, 1);
    assert_eq!(summary["hosted"].upload_count, 1);
    assert_eq!(summary["root/hosted"].project_count, 1);
    assert_eq!(summary["root/hosted"].upload_count, 2);
    // Newest upload-time first.
    let recent: Vec<&str> = summary["root/hosted"]
        .recent_uploads
        .iter()
        .map(|upload| upload.filename.as_str())
        .collect();
    assert_eq!(recent, vec!["django-4.0.whl", "django-3.2.whl"]);
}

#[test]
fn test_summarize_indexes_breaks_an_upload_time_tie_by_filename() {
    let (_dir, meta) = store();
    // Same upload-time on both, so the sort falls through to the filename tiebreak.
    upload(
        &meta,
        "hosted",
        "flask",
        "flask-2.0.whl",
        "2.0",
        "2026-01-01T00:00:00Z",
        10,
    );
    upload(
        &meta,
        "hosted",
        "flask",
        "flask-1.0.whl",
        "1.0",
        "2026-01-01T00:00:00Z",
        10,
    );
    let summary = meta.summarize_indexes(&["hosted".to_owned()], 5).unwrap();
    let recent: Vec<&str> = summary["hosted"]
        .recent_uploads
        .iter()
        .map(|upload| upload.filename.as_str())
        .collect();
    assert_eq!(recent, vec!["flask-1.0.whl", "flask-2.0.whl"]);
}

#[test]
fn test_summarize_indexes_truncates_recent_to_the_limit() {
    let (_dir, meta) = store();
    upload(
        &meta,
        "hosted",
        "flask",
        "flask-2.0.whl",
        "2.0",
        "2026-02-01T00:00:00Z",
        10,
    );
    upload(
        &meta,
        "hosted",
        "flask",
        "flask-1.0.whl",
        "1.0",
        "2026-01-01T00:00:00Z",
        10,
    );
    let summary = meta.summarize_indexes(&["hosted".to_owned()], 1).unwrap();
    assert_eq!(summary["hosted"].recent_uploads.len(), 1);
    assert_eq!(summary["hosted"].recent_uploads[0].filename, "flask-2.0.whl");
}

#[test]
fn test_summarize_indexes_with_a_zero_limit_counts_but_keeps_no_recent() {
    let (_dir, meta) = store();
    upload(
        &meta,
        "hosted",
        "flask",
        "flask-1.0.whl",
        "1.0",
        "2026-01-01T00:00:00Z",
        10,
    );
    let summary = meta.summarize_indexes(&["hosted".to_owned()], 0).unwrap();
    assert_eq!(summary["hosted"].upload_count, 1);
    assert!(summary["hosted"].recent_uploads.is_empty());
}

#[test]
fn test_summarize_indexes_counts_an_unparsable_upload_without_a_recent_entry() {
    let (_dir, meta) = store();
    // A stored upload whose body is not valid JSON still counts, but contributes no recent entry.
    meta.put_upload("hosted", "flask", "flask-1.0.whl", b"not json")
        .unwrap();
    let summary = meta.summarize_indexes(&["hosted".to_owned()], 5).unwrap();
    assert_eq!(summary["hosted"].upload_count, 1);
    assert!(summary["hosted"].recent_uploads.is_empty());
}

#[test]
fn test_summarize_indexes_skips_a_malformed_upload_key() {
    let (_dir, meta) = store();
    // A row whose key carries no project/filename split is skipped rather than counted.
    meta.put_driver_value(&format!("{UPLOAD_PREFIX}hosted/onlyproject"), b"{}")
        .unwrap();
    let summary = meta.summarize_indexes(&["hosted".to_owned()], 5).unwrap();
    assert_eq!(summary["hosted"].upload_count, 0);
}
