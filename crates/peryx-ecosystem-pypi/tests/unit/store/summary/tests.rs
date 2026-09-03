use std::collections::BTreeMap;

use peryx_driver::serving::{IndexSummaryDriver as _, IndexSummaryError};
use peryx_storage::meta::MetaError;

use super::MetaStore;
use crate::store::{Guard, PromotedRelease, PypiStore as _, UploadMutation};

/// One index's count row, written by every project and upload change.
const COUNT_KEY: &str = "pypi\u{0}k\u{0}hosted";
/// The order range of the `hosted` index; a row's remaining segments only position it.
const ORDER_KEY: &str = "pypi\u{0}w\u{0}hosted\u{0}damaged";

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, meta)
}

fn record(filename: &str, version: &str, at: &str, size: u64) -> String {
    format!(r#"{{"version":"{version}","file":{{"filename":"{filename}","upload-time":"{at}","size":{size}}}}}"#)
}

fn upload(meta: &MetaStore, index: &str, project: &str, filename: &str, version: &str, at: &str, size: u64) {
    meta.put_upload(index, project, filename, record(filename, version, at, size).as_bytes())
        .unwrap();
}

fn artifacts(meta: &MetaStore, index: &str, limit: usize) -> Vec<String> {
    meta.summarize_indexes(&[index.to_owned()], limit).unwrap()[index]
        .recent_writes
        .iter()
        .map(|write| write.artifact.clone())
        .collect()
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

    assert_eq!(summary["hosted"].resource_count, 1);
    assert_eq!(summary["hosted"].write_count, 1);
    assert_eq!(summary["root/hosted"].resource_count, 1);
    assert_eq!(summary["root/hosted"].write_count, 2);
    assert_eq!(
        artifacts(&meta, "root/hosted", 5),
        vec!["django-4.0.whl", "django-3.2.whl"]
    );
}

/// `root` and `root/hosted` are separate indexes whose keys share a leading segment, so the shorter
/// name's order range must stop before the longer name's rows.
#[test]
fn test_summarize_indexes_keeps_a_name_prefixed_index_out_of_the_shorter_range() {
    let (_dir, meta) = store();
    upload(
        &meta,
        "root",
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

    let summary = meta
        .summarize_indexes(&["root".to_owned(), "root/hosted".to_owned()], 5)
        .unwrap();
    assert_eq!(summary["root"].write_count, 1);
    assert_eq!(artifacts(&meta, "root", 5), vec!["flask-1.0.whl"]);
    assert_eq!(artifacts(&meta, "root/hosted", 5), vec!["django-4.0.whl"]);
}

#[test]
fn test_summarize_indexes_breaks_an_upload_time_tie_by_filename() {
    let (_dir, meta) = store();
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

    assert_eq!(artifacts(&meta, "hosted", 5), vec!["flask-1.0.whl", "flask-2.0.whl"]);
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

    assert_eq!(artifacts(&meta, "hosted", 1), vec!["flask-2.0.whl"]);
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
    assert_eq!(summary["hosted"].write_count, 1);
    assert!(summary["hosted"].recent_writes.is_empty());
}

#[test]
fn test_summarize_indexes_reports_an_index_with_no_rows_as_empty() {
    let (_dir, meta) = store();

    let summary = meta.summarize_indexes(&["hosted".to_owned()], 5).unwrap();
    assert_eq!(summary["hosted"].resource_count, 0);
    assert_eq!(summary["hosted"].write_count, 0);
    assert!(summary["hosted"].recent_writes.is_empty());
}

#[test]
fn test_summarize_indexes_counts_an_unparsable_upload_without_a_recent_entry() {
    let (_dir, meta) = store();

    meta.put_upload("hosted", "flask", "flask-1.0.whl", b"not json")
        .unwrap();

    let summary = meta.summarize_indexes(&["hosted".to_owned()], 5).unwrap();
    assert_eq!(summary["hosted"].write_count, 1);
    assert!(summary["hosted"].recent_writes.is_empty());
}

/// A time peryx cannot read as RFC 3339 is where a missing one already sat: behind every readable one.
#[test]
fn test_summarize_indexes_sorts_an_unreadable_upload_time_last() {
    let (_dir, meta) = store();
    meta.put_upload(
        "hosted",
        "flask",
        "undated.whl",
        br#"{"file":{"filename":"undated.whl"}}"#,
    )
    .unwrap();
    meta.put_upload(
        "hosted",
        "flask",
        "damaged.whl",
        br#"{"file":{"filename":"damaged.whl","upload-time":"yesterday"}}"#,
    )
    .unwrap();
    upload(&meta, "hosted", "flask", "dated.whl", "1.0", "1970-01-01T00:00:00Z", 10);

    assert_eq!(
        artifacts(&meta, "hosted", 5),
        vec!["dated.whl", "damaged.whl", "undated.whl"]
    );
}

#[test]
fn test_summarize_indexes_reports_the_stored_upload_fields() {
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

    let summary = meta.summarize_indexes(&["hosted".to_owned()], 5).unwrap();
    assert_eq!(
        summary["hosted"].recent_writes,
        vec![peryx_driver::serving::RecentWrite {
            resource: "flask".to_owned(),
            artifact: "flask-1.0.whl".to_owned(),
            group: "1.0".to_owned(),
            written_at: Some("2026-01-01T00:00:00Z".to_owned()),
            size: Some(10),
        }]
    );
}

#[test]
fn test_republishing_an_upload_counts_it_once() {
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
    upload(
        &meta,
        "hosted",
        "flask",
        "flask-1.0.whl",
        "1.0",
        "2026-03-01T00:00:00Z",
        11,
    );

    let summary = meta.summarize_indexes(&["hosted".to_owned()], 5).unwrap();
    assert_eq!(summary["hosted"].write_count, 1);
    assert_eq!(summary["hosted"].recent_writes.len(), 1);
    assert_eq!(
        summary["hosted"].recent_writes[0].written_at,
        Some("2026-03-01T00:00:00Z".to_owned())
    );
}

/// A record that never parsed held no position, so replacing it adds one rather than moving one.
#[test]
fn test_replacing_an_unparsable_upload_gives_it_a_recent_entry() {
    let (_dir, meta) = store();
    meta.put_upload("hosted", "flask", "flask-1.0.whl", b"not json")
        .unwrap();
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
    assert_eq!(summary["hosted"].write_count, 1);
    assert_eq!(artifacts(&meta, "hosted", 5), vec!["flask-1.0.whl"]);
}

#[test]
fn test_rewriting_an_upload_moves_its_recent_entry() {
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
    upload(
        &meta,
        "hosted",
        "flask",
        "flask-2.0.whl",
        "2.0",
        "2026-02-01T00:00:00Z",
        10,
    );
    meta.mutate_uploads(false, "hosted", "flask", "update", 0, |filename, _record| {
        Ok::<_, MetaError>(match filename {
            "flask-1.0.whl" => {
                UploadMutation::Replace(record(filename, "1.0", "2026-03-01T00:00:00Z", 10).into_bytes())
            }
            _ => UploadMutation::Keep,
        })
    })
    .unwrap();

    let summary = meta.summarize_indexes(&["hosted".to_owned()], 5).unwrap();
    assert_eq!(summary["hosted"].write_count, 2);
    assert_eq!(artifacts(&meta, "hosted", 5), vec!["flask-1.0.whl", "flask-2.0.whl"]);
}

#[test]
fn test_deleting_an_upload_drops_its_count_and_recent_entry() {
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
    upload(
        &meta,
        "hosted",
        "flask",
        "flask-2.0.whl",
        "2.0",
        "2026-02-01T00:00:00Z",
        10,
    );
    assert!(
        meta.delete_upload(false, "hosted", "flask", "flask-2.0.whl", 0)
            .unwrap()
    );

    let summary = meta.summarize_indexes(&["hosted".to_owned()], 5).unwrap();
    assert_eq!(summary["hosted"].write_count, 1);
    assert_eq!(artifacts(&meta, "hosted", 5), vec!["flask-1.0.whl"]);
}

/// The count row goes with the last row it counted, so a drained index reads as empty rather than as a
/// pair of zeroes nothing writes again.
#[test]
fn test_deleting_every_upload_leaves_the_index_empty() {
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
    meta.mutate_uploads(false, "hosted", "flask", "delete-file", 0, |_filename, _record| {
        Ok::<_, MetaError>(UploadMutation::Delete)
    })
    .unwrap();

    let summary = meta.summarize_indexes(&["hosted".to_owned()], 5).unwrap();
    assert_eq!(summary["hosted"].write_count, 0);
    assert!(summary["hosted"].recent_writes.is_empty());
}

#[test]
fn test_promoting_a_release_counts_its_files_and_project() {
    let (_dir, meta) = store();
    let records = vec![(
        "flask-1.0.whl".to_owned(),
        "0".repeat(64),
        record("flask-1.0.whl", "1.0", "2026-01-01T00:00:00Z", 10).into_bytes(),
    )];
    let promoted = meta
        .promote_files_checked::<MetaError>(
            false,
            &PromotedRelease {
                source: "staging",
                index: "hosted",
                normalized: "flask",
                display: "Flask",
                records: &records,
                blob_sizes: &BTreeMap::new(),
                reservations: &BTreeMap::new(),
                submitted_at_unix: 0,
            },
            |_filename, _token, _stored| Ok(Guard::Commit),
        )
        .unwrap();

    assert_eq!(promoted, 1);
    let summary = meta.summarize_indexes(&["hosted".to_owned()], 5).unwrap();
    assert_eq!(summary["hosted"].resource_count, 1);
    assert_eq!(summary["hosted"].write_count, 1);
    assert_eq!(artifacts(&meta, "hosted", 5), vec!["flask-1.0.whl"]);
}

#[test]
fn test_summary_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    {
        let meta = MetaStore::open(&path).unwrap();
        meta.put_project("hosted", "flask", "Flask").unwrap();
        upload(
            &meta,
            "hosted",
            "flask",
            "flask-1.0.whl",
            "1.0",
            "2026-01-01T00:00:00Z",
            10,
        );
    }

    let meta = MetaStore::open(&path).unwrap();
    let summary = meta.summarize_indexes(&["hosted".to_owned()], 5).unwrap();
    assert_eq!(summary["hosted"].resource_count, 1);
    assert_eq!(summary["hosted"].write_count, 1);
    assert_eq!(artifacts(&meta, "hosted", 5), vec!["flask-1.0.whl"]);
}

#[test]
fn test_summarize_indexes_rejects_a_count_row_missing_its_upload_field() {
    let (_dir, meta) = store();
    meta.put_driver_value(COUNT_KEY, b"1").unwrap();

    assert_eq!(
        meta.summarize_indexes(&["hosted".to_owned()], 5)
            .unwrap_err()
            .to_string(),
        format!("driver record {COUNT_KEY:?} is missing field \"uploads\"")
    );
}

#[test]
fn test_summarize_indexes_rejects_a_count_row_with_an_unreadable_number() {
    let (_dir, meta) = store();
    meta.put_driver_value(COUNT_KEY, b"many\n1").unwrap();

    assert_eq!(
        meta.summarize_indexes(&["hosted".to_owned()], 5)
            .unwrap_err()
            .to_string(),
        format!("driver record {COUNT_KEY:?} has invalid integer field \"projects\"")
    );
}

#[test]
fn test_summarize_indexes_rejects_a_count_row_that_is_not_utf8() {
    let (_dir, meta) = store();
    meta.put_driver_value(COUNT_KEY, &[0xff, 0xff]).unwrap();

    assert_eq!(
        meta.summarize_indexes(&["hosted".to_owned()], 5)
            .unwrap_err()
            .to_string(),
        format!("driver record {COUNT_KEY:?} is not UTF-8")
    );
}

#[test]
fn test_summarize_indexes_rejects_a_damaged_recent_row() {
    let (_dir, meta) = store();
    meta.put_driver_value(ORDER_KEY, b"not json").unwrap();

    assert_eq!(
        meta.summarize_indexes(&["hosted".to_owned()], 5)
            .unwrap_err()
            .to_string(),
        format!("driver record {ORDER_KEY:?} does not decode")
    );
}

#[test]
fn test_summary_driver_classifies_storage_failures() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("peryx.redb");
    drop(MetaStore::open(&path).unwrap());
    let database = redb::Database::open(&path).unwrap();
    let write = database.begin_write().unwrap();
    write
        .delete_table(redb::TableDefinition::<&str, &[u8]>::new("driver_kv"))
        .unwrap();
    write
        .open_table(redb::TableDefinition::<&str, &str>::new("driver_kv"))
        .unwrap();
    write.commit().unwrap();
    drop(database);

    assert_eq!(
        crate::PypiServing.summarize_indexes(&MetaStore::open_existing(path).unwrap(), &["hosted".to_owned()], 5),
        Err(IndexSummaryError::Storage)
    );
}

/// A cached index's project markers are written by whichever node fetched the page, so their count is
/// maintained on the same local footing and a purge takes it back down.
#[test]
fn test_caching_then_purging_a_project_counts_it_and_gives_it_back() {
    let (_dir, meta) = store();
    meta.put_cached_page(crate::store::CachedPageWrite {
        key: "pypi/flask",
        record: &crate::store::CachedIndex {
            source: None,
            last_modified: None,
            etag: None,
            last_serial: None,
            fetched_at_unix: 1,
            content_type: None,
            fresh_secs: None,
            body: Vec::new(),
        },
        index: "pypi",
        normalized: "flask",
        display: "Flask",
        source: "pypi",
        upstream: None,
        project_status: None,
        project_status_reason: None,
        files: &[],
        attestations: &[],
    })
    .unwrap();
    let cached = meta.summarize_indexes(&["pypi".to_owned()], 5).unwrap();

    meta.delete_project_cache("pypi", "flask", &[], &[]).unwrap();
    let purged = meta.summarize_indexes(&["pypi".to_owned()], 5).unwrap();

    assert_eq!(cached["pypi"].resource_count, 1);
    assert_eq!(purged["pypi"].resource_count, 0);
}
