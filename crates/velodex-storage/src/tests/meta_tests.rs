use std::collections::HashMap;
use std::error::Error as _;

use crate::meta::{
    CachedIndex, FileSource, JournalEntry, MetaError, MetaScanError, MetaStore, NewWebhookDelivery,
    WebhookDeliveryAttempt, WebhookDeliveryStatus,
};

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    (dir, store)
}

fn record() -> CachedIndex {
    CachedIndex {
        etag: Some("\"abc\"".to_owned()),
        last_serial: Some(42),
        fetched_at_unix: 1_700_000_000,
        content_type: None,

        fresh_secs: None,
        body: b"<html></html>".to_vec(),
    }
}

#[test]
fn test_serial_starts_at_zero_and_increments() {
    let (_dir, store) = store();
    assert_eq!(store.current_serial().unwrap(), 0);
    assert_eq!(store.next_serial().unwrap(), 1);
    assert_eq!(store.next_serial().unwrap(), 2);
    assert_eq!(store.current_serial().unwrap(), 2);
}

#[test]
fn test_journal_appends_entries_and_reads_the_changelog() {
    let (_dir, store) = store();
    assert_eq!(
        store
            .append_journal("add-file", "flask", Some("1.0"), Some("flask-1.0.whl"))
            .unwrap(),
        1
    );
    assert_eq!(store.append_journal("promote", "flask", None, None).unwrap(), 2);
    assert_eq!(store.current_serial().unwrap(), 2);

    let all = store.journal_since(0).unwrap();
    assert_eq!(
        all,
        vec![
            JournalEntry {
                serial: 1,
                action: "add-file".to_owned(),
                project: "flask".to_owned(),
                version: Some("1.0".to_owned()),
                filename: Some("flask-1.0.whl".to_owned()),
            },
            JournalEntry {
                serial: 2,
                action: "promote".to_owned(),
                project: "flask".to_owned(),
                version: None,
                filename: None,
            },
        ]
    );

    let tail = store.journal_since(1).unwrap();
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].serial, 2);
}

#[test]
fn test_put_and_get_index_roundtrip() {
    let (_dir, store) = store();
    assert_eq!(store.get_index("root/pypi/flask").unwrap(), None);
    store.put_index("root/pypi/flask", &record()).unwrap();
    assert_eq!(store.get_index("root/pypi/flask").unwrap(), Some(record()));
}

#[test]
fn test_put_index_overwrites() {
    let (_dir, store) = store();
    store.put_index("k", &record()).unwrap();
    let mut updated = record();
    updated.last_serial = Some(99);
    store.put_index("k", &updated).unwrap();
    assert_eq!(store.get_index("k").unwrap().unwrap().last_serial, Some(99));
}

#[test]
fn test_reopen_persists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("velodex.redb");
    {
        let store = MetaStore::open(&path).unwrap();
        store.next_serial().unwrap();
        store.put_index("k", &record()).unwrap();
    }
    let reopened = MetaStore::open(&path).unwrap();
    assert_eq!(reopened.current_serial().unwrap(), 1);
    assert_eq!(reopened.get_index("k").unwrap(), Some(record()));
}

#[test]
fn test_webhook_delivery_queue_orders_due_records() {
    let (_dir, store) = store();
    let later = store
        .enqueue_webhook_delivery(NewWebhookDelivery {
            index: "local",
            target: "ci",
            event: "upload",
            payload: r#"{"event":"upload"}"#,
            created_at_unix: 20,
        })
        .unwrap();
    let earlier = store
        .enqueue_webhook_delivery(NewWebhookDelivery {
            index: "local",
            target: "ci",
            event: "delete",
            payload: r#"{"event":"delete"}"#,
            created_at_unix: 10,
        })
        .unwrap();

    assert_eq!(store.next_webhook_delivery_at().unwrap(), Some(10));
    assert_eq!(store.list_due_webhook_deliveries(9, 10).unwrap(), Vec::new());
    let due = store.list_due_webhook_deliveries(20, 1).unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].id, earlier);
    assert_eq!(store.get_webhook_delivery(&later).unwrap().unwrap().event, "upload");
}

#[test]
fn test_webhook_delivery_update_reschedules_and_finishes() {
    let (_dir, store) = store();
    let id = store
        .enqueue_webhook_delivery(NewWebhookDelivery {
            index: "local",
            target: "ci",
            event: "upload",
            payload: r#"{"event":"upload"}"#,
            created_at_unix: 10,
        })
        .unwrap();

    let pending = store
        .update_webhook_delivery(
            &id,
            WebhookDeliveryAttempt {
                status: WebhookDeliveryStatus::Pending,
                updated_at_unix: 11,
                next_attempt_at_unix: Some(16),
                response_status: Some(500),
                last_error: Some("http status 500"),
            },
        )
        .unwrap()
        .unwrap();

    assert_eq!(pending.attempts, 1);
    assert_eq!(pending.next_attempt_at_unix, Some(16));
    assert_eq!(store.next_webhook_delivery_at().unwrap(), Some(16));
    assert!(store.list_due_webhook_deliveries(15, 10).unwrap().is_empty());
    assert_eq!(store.list_due_webhook_deliveries(16, 10).unwrap()[0].id, id);

    let delivered = store
        .update_webhook_delivery(
            &id,
            WebhookDeliveryAttempt {
                status: WebhookDeliveryStatus::Delivered,
                updated_at_unix: 16,
                next_attempt_at_unix: None,
                response_status: Some(204),
                last_error: None,
            },
        )
        .unwrap()
        .unwrap();

    assert_eq!(delivered.attempts, 2);
    assert_eq!(delivered.status, WebhookDeliveryStatus::Delivered);
    assert_eq!(store.next_webhook_delivery_at().unwrap(), None);
    assert!(store.list_due_webhook_deliveries(100, 10).unwrap().is_empty());
}

#[test]
fn test_webhook_delivery_update_handles_record_without_due_key() {
    let (_dir, store) = store();
    let id = store
        .enqueue_webhook_delivery(NewWebhookDelivery {
            index: "local",
            target: "ci",
            event: "upload",
            payload: r#"{"event":"upload"}"#,
            created_at_unix: 10,
        })
        .unwrap();

    store
        .update_webhook_delivery(
            &id,
            WebhookDeliveryAttempt {
                status: WebhookDeliveryStatus::Delivered,
                updated_at_unix: 11,
                next_attempt_at_unix: None,
                response_status: Some(204),
                last_error: None,
            },
        )
        .unwrap();
    let failed = store
        .update_webhook_delivery(
            &id,
            WebhookDeliveryAttempt {
                status: WebhookDeliveryStatus::Failed,
                updated_at_unix: 12,
                next_attempt_at_unix: None,
                response_status: None,
                last_error: Some("manual terminal update"),
            },
        )
        .unwrap()
        .unwrap();

    assert_eq!(failed.attempts, 2);
    assert_eq!(failed.status, WebhookDeliveryStatus::Failed);
    assert_eq!(failed.next_attempt_at_unix, None);
    assert_eq!(failed.last_error.as_deref(), Some("manual terminal update"));
    assert_eq!(store.next_webhook_delivery_at().unwrap(), None);
}

#[test]
fn test_webhook_delivery_ignores_empty_limit_and_stale_due_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("velodex.redb");
    let store = MetaStore::open(&path).unwrap();
    assert!(store.list_due_webhook_deliveries(10, 0).unwrap().is_empty());
    assert!(
        store
            .update_webhook_delivery(
                "missing",
                WebhookDeliveryAttempt {
                    status: WebhookDeliveryStatus::Delivered,
                    updated_at_unix: 10,
                    next_attempt_at_unix: None,
                    response_status: Some(204),
                    last_error: None,
                },
            )
            .unwrap()
            .is_none()
    );
    drop(store);

    let db = redb::Database::create(&path).unwrap();
    let table: redb::TableDefinition<&str, &str> = redb::TableDefinition::new("webhook_due");
    let txn = db.begin_write().unwrap();
    {
        let mut table = txn.open_table(table).unwrap();
        table.insert("not-a-due-key", "missing").unwrap();
        table.insert("09223372036854775808/missing", "missing").unwrap();
    }
    txn.commit().unwrap();
    drop(db);

    let store = MetaStore::open(&path).unwrap();
    assert!(store.list_due_webhook_deliveries(0, 10).unwrap().is_empty());
}

#[test]
fn test_put_and_get_file_url() {
    let (_dir, store) = store();
    assert_eq!(store.get_file_url("deadbeef").unwrap(), None);
    store
        .put_file_url("deadbeef", "https://files.example/pkg.whl", "pypi")
        .unwrap();
    assert_eq!(
        store.get_file_url("deadbeef").unwrap(),
        Some(FileSource {
            url: "https://files.example/pkg.whl".to_owned(),
            source: "pypi".to_owned(),
            size: None,
        })
    );
}

#[test]
fn test_put_mirror_page_records_file_url_size() {
    let (_dir, store) = store();
    store
        .put_mirror_page(
            "pypi/pkg",
            &record(),
            "pypi",
            "pkg",
            "Pkg",
            "pypi",
            None,
            None,
            &[(
                "feedface".to_owned(),
                "https://files.example/pkg-1.0.whl".to_owned(),
                Some(42),
            )],
            &[],
        )
        .unwrap();

    assert_eq!(
        store.get_file_url("feedface").unwrap(),
        Some(FileSource {
            url: "https://files.example/pkg-1.0.whl".to_owned(),
            source: "pypi".to_owned(),
            size: Some(42),
        })
    );
}

#[test]
fn test_put_and_get_metadata() {
    let (_dir, store) = store();
    assert_eq!(store.get_metadata("wheelsha").unwrap(), None);
    store
        .put_metadata("wheelsha", "https://up/pkg.whl.metadata", "metasha", "pypi")
        .unwrap();
    assert_eq!(
        store.get_metadata("wheelsha").unwrap(),
        Some((
            "https://up/pkg.whl.metadata".to_owned(),
            "metasha".to_owned(),
            "pypi".to_owned()
        ))
    );
}

#[test]
fn test_get_metadata_digests_skips_missing_and_malformed_records() {
    let (dir, store) = store();
    store
        .put_metadata("wheelsha", "https://up/pkg.whl.metadata", "metasha", "pypi")
        .unwrap();
    drop(store);
    {
        let db = redb::Database::create(dir.path().join("velodex.redb")).unwrap();
        let table: redb::TableDefinition<&str, &str> = redb::TableDefinition::new("metadata");
        let txn = db.begin_write().unwrap();
        txn.open_table(table).unwrap().insert("broken", "only-url").unwrap();
        txn.commit().unwrap();
    }
    let store = MetaStore::open(dir.path().join("velodex.redb")).unwrap();

    let digests = store.get_metadata_digests(["missing", "broken", "wheelsha"]).unwrap();

    assert_eq!(digests, HashMap::from([("wheelsha".to_owned(), "metasha".to_owned())]));
}

#[test]
fn test_put_and_list_projects() {
    let (_dir, store) = store();
    assert!(store.list_projects("root/pypi").unwrap().is_empty());
    store.put_project("root/pypi", "flask", "Flask").unwrap();
    store.put_project("root/pypi", "django", "Django").unwrap();
    store.put_project("other/index", "x", "X").unwrap();
    store.put_project("root/pypi", "flask", "Flask").unwrap(); // re-observe, no duplicate
    assert_eq!(store.list_projects("root/pypi").unwrap(), vec!["Django", "Flask"]);
}

#[test]
fn test_put_and_list_upload_entries() {
    let (_dir, store) = store();
    assert!(store.list_upload_entries("root/local", "flask").unwrap().is_empty());
    store
        .put_upload("root/local", "flask", "flask-2.0.whl", b"two")
        .unwrap();
    store
        .put_upload("root/local", "flask", "flask-1.0.whl", b"one")
        .unwrap();
    store
        .put_upload("root/local", "django", "django-4.0.whl", b"other")
        .unwrap();
    // Sorted by filename, scoped to the one project, filename returned alongside the record.
    assert_eq!(
        store.list_upload_entries("root/local", "flask").unwrap(),
        vec![
            ("flask-1.0.whl".to_owned(), b"one".to_vec()),
            ("flask-2.0.whl".to_owned(), b"two".to_vec())
        ]
    );
    assert_eq!(
        store.list_upload_entries("root/local", "django").unwrap(),
        vec![("django-4.0.whl".to_owned(), b"other".to_vec())]
    );
}

#[test]
fn test_put_uploads_stores_records_and_project_display() {
    let (_dir, store) = store();
    let records = vec![
        ("flask-1.0.whl".to_owned(), b"one".to_vec()),
        ("flask-1.0.tar.gz".to_owned(), b"sdist".to_vec()),
    ];

    store.put_uploads("prod", "flask", "Flask", &records).unwrap();

    assert_eq!(store.get_project("prod", "flask").unwrap().as_deref(), Some("Flask"));
    assert_eq!(
        store.list_upload_entries("prod", "flask").unwrap(),
        vec![
            ("flask-1.0.tar.gz".to_owned(), b"sdist".to_vec()),
            ("flask-1.0.whl".to_owned(), b"one".to_vec())
        ]
    );
}

#[test]
fn test_summarize_indexes_counts_projects_and_recent_uploads() {
    let (_dir, store) = store();
    store.put_project("local", "flask", "Flask").unwrap();
    store.put_project("root/local", "django", "Django").unwrap();
    store
        .put_upload(
            "local",
            "flask",
            "flask-1.0.whl",
            br#"{"version":"1.0","file":{"filename":"flask-1.0.whl","upload-time":"2026-01-01T00:00:00Z","size":10}}"#,
        )
        .unwrap();
    store
        .put_upload(
            "root/local",
            "django",
            "django-4.0.whl",
            br#"{"version":"4.0","file":{"filename":"django-4.0.whl","upload-time":"2026-02-01T00:00:00Z","size":20}}"#,
        )
        .unwrap();
    store
        .put_upload(
            "root/local",
            "django",
            "django-4.1.whl",
            br#"{"version":"4.1","file":{"filename":"django-4.1.whl","upload-time":"2026-02-01T00:00:00Z","size":21}}"#,
        )
        .unwrap();
    store
        .put_upload(
            "root/local",
            "django",
            "django-3.2.whl",
            br#"{"version":"3.2","file":{"filename":"django-3.2.whl","upload-time":"2025-12-01T00:00:00Z","size":15}}"#,
        )
        .unwrap();
    store
        .put_upload("foreign", "flask", "ignored.whl", br#"{"version":"1.0"}"#)
        .unwrap();

    let indexes = vec!["local".to_owned(), "root/local".to_owned()];
    let summary = store.summarize_indexes(&indexes, 1).unwrap();

    assert_eq!(summary["local"].project_count, 1);
    assert_eq!(summary["local"].upload_count, 1);
    assert_eq!(summary["root/local"].project_count, 1);
    assert_eq!(summary["root/local"].upload_count, 3);
    assert_eq!(summary["root/local"].recent_uploads[0].filename, "django-4.0.whl");

    let summary = store.summarize_indexes(&indexes, 0).unwrap();
    assert_eq!(summary["root/local"].upload_count, 3);
    assert!(summary["root/local"].recent_uploads.is_empty());
}

#[test]
fn test_put_upload_overwrites_same_filename() {
    let (_dir, store) = store();
    store
        .put_upload("root/local", "flask", "flask-1.0.whl", b"first")
        .unwrap();
    store
        .put_upload("root/local", "flask", "flask-1.0.whl", b"second")
        .unwrap();
    assert_eq!(
        store.list_upload_entries("root/local", "flask").unwrap(),
        vec![("flask-1.0.whl".to_owned(), b"second".to_vec())]
    );
}

#[test]
fn test_get_upload_fetches_one_entry() {
    let (_dir, store) = store();
    store
        .put_upload("root/local", "flask", "flask-1.0.whl", b"one")
        .unwrap();
    assert_eq!(
        store
            .get_upload("root/local", "flask", "flask-1.0.whl")
            .unwrap()
            .as_deref(),
        Some(b"one".as_slice())
    );
    assert!(
        store
            .get_upload("root/local", "flask", "missing.whl")
            .unwrap()
            .is_none()
    );
}

#[test]
fn test_delete_upload() {
    let (_dir, store) = store();
    store
        .put_upload("root/local", "flask", "flask-1.0.whl", b"one")
        .unwrap();
    assert!(store.delete_upload("root/local", "flask", "flask-1.0.whl").unwrap());
    assert!(!store.delete_upload("root/local", "flask", "flask-1.0.whl").unwrap());
    assert!(store.list_upload_entries("root/local", "flask").unwrap().is_empty());
}

#[test]
fn test_cached_index_encode_decode_roundtrip() {
    assert_eq!(CachedIndex::decode(&record().encode()).unwrap(), record());
}

#[test]
fn test_cached_index_decode_rejects_garbage() {
    assert!(CachedIndex::decode(b"not json").is_err());
}

#[test]
fn test_put_list_and_delete_overrides() {
    let (_dir, store) = store();
    assert!(store.list_overrides("local", "flask").unwrap().is_empty());
    store.put_override("local", "flask", "flask-1.0.whl", "yanked").unwrap();
    store.put_override("local", "flask", "flask-2.0.whl", "hidden").unwrap();
    store.put_override("local", "other", "x.whl", "hidden").unwrap();
    assert_eq!(
        store.list_overrides("local", "flask").unwrap(),
        vec![
            ("flask-1.0.whl".to_owned(), "yanked".to_owned()),
            ("flask-2.0.whl".to_owned(), "hidden".to_owned())
        ]
    );
    assert!(store.delete_override("local", "flask", "flask-1.0.whl").unwrap());
    assert!(!store.delete_override("local", "flask", "flask-1.0.whl").unwrap());
}

#[test]
fn test_put_override_replaces_kind() {
    let (_dir, store) = store();
    store.put_override("local", "flask", "flask-1.0.whl", "yanked").unwrap();
    store.put_override("local", "flask", "flask-1.0.whl", "hidden").unwrap();
    assert_eq!(
        store.list_overrides("local", "flask").unwrap(),
        vec![("flask-1.0.whl".to_owned(), "hidden".to_owned())]
    );
}

#[test]
fn test_encode_decode_roundtrips_framed_record() {
    let original = CachedIndex {
        fresh_secs: Some(600),
        ..record()
    };
    let bytes = original.encode();
    assert!(bytes.starts_with(b"velodex1\n"));
    assert!(bytes.ends_with(b"<html></html>"));
    assert_eq!(CachedIndex::decode(&bytes).unwrap(), original);
}

#[test]
fn test_decode_accepts_legacy_json_records() {
    let legacy = serde_json::to_vec(&record()).unwrap();
    assert_eq!(CachedIndex::decode(&legacy).unwrap(), record());
}

#[test]
fn test_list_index_pages_reports_freshness() {
    let (_dir, store) = store();
    store.put_index("pypi/flask", &record()).unwrap();
    store
        .put_index(
            "pypi/numpy",
            &CachedIndex {
                fetched_at_unix: 1_800_000_000,
                fresh_secs: Some(600),
                ..record()
            },
        )
        .unwrap();
    let mut pages = store.list_index_pages().unwrap();
    pages.sort();
    assert_eq!(
        pages,
        vec![
            ("pypi/flask".to_owned(), 1_700_000_000, None),
            ("pypi/numpy".to_owned(), 1_800_000_000, Some(600)),
        ]
    );
}

#[test]
fn test_list_index_pages_reads_legacy_records() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("velodex.redb");
    MetaStore::open(&path).unwrap();
    // A record written by a version that stored the whole struct as plain JSON.
    let legacy = serde_json::to_vec(&record()).unwrap();
    let db = redb::Database::create(&path).unwrap();
    let table: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("simple_index");
    let txn = db.begin_write().unwrap();
    txn.open_table(table)
        .unwrap()
        .insert("pypi/old", legacy.as_slice())
        .unwrap();
    txn.commit().unwrap();
    drop(db);
    let store = MetaStore::open(&path).unwrap();
    assert_eq!(
        store.list_index_pages().unwrap(),
        vec![("pypi/old".to_owned(), 1_700_000_000, None)]
    );
}

#[test]
fn test_cached_index_summary_reports_body_and_record_size() {
    let bytes = record().encode();
    assert_eq!(
        CachedIndex::summary(&bytes).unwrap(),
        crate::meta::CachedIndexSummary {
            fetched_at_unix: 1_700_000_000,
            fresh_secs: None,
            body_bytes: 13,
            record_bytes: bytes.len() as u64,
            last_serial: Some(42),
            content_type: None,
        }
    );
}

#[test]
fn test_scan_index_pages_visits_records_without_collecting() {
    let (_dir, store) = store();
    store.put_index("pypi/flask", &record()).unwrap();
    let mut pages = Vec::new();
    store
        .scan_index_pages(|page| {
            pages.push((page.key, page.summary.body_bytes));
            Ok::<(), std::io::Error>(())
        })
        .unwrap();
    assert_eq!(pages, vec![("pypi/flask".to_owned(), 13)]);
}

#[test]
fn test_scan_visit_error_reports_source() {
    let (_dir, store) = store();
    store.put_index("pypi/flask", &record()).unwrap();
    let err = store
        .scan_index_pages(|_page| Err(std::io::Error::other("stop")))
        .unwrap_err();
    assert_eq!(err.to_string(), "stop");
    assert!(err.source().is_some());
}

#[test]
fn test_scan_store_error_reports_source() {
    let decode = serde_json::from_slice::<serde_json::Value>(b"not json").unwrap_err();
    let err: MetaScanError<std::io::Error> = MetaError::Decode(decode).into();
    assert!(!err.to_string().is_empty());
    assert!(err.source().is_some());
}

#[test]
fn test_scan_upload_and_override_records_visit_rows() {
    let (_dir, store) = store();
    store.put_upload("local", "flask", "flask-1.0.whl", b"upload").unwrap();
    store.put_override("local", "flask", "flask-1.0.whl", "hidden").unwrap();

    let mut uploads = Vec::new();
    store
        .scan_upload_records(|key, value| {
            uploads.push((key.to_owned(), value.to_vec()));
            Ok::<(), std::io::Error>(())
        })
        .unwrap();
    assert_eq!(
        uploads,
        vec![("local/flask/flask-1.0.whl".to_owned(), b"upload".to_vec())]
    );

    let mut overrides = Vec::new();
    store
        .scan_override_records(|key, value| {
            overrides.push((key.to_owned(), value.to_owned()));
            Ok::<(), std::io::Error>(())
        })
        .unwrap();
    assert_eq!(
        overrides,
        vec![("local/flask/flask-1.0.whl".to_owned(), "hidden".to_owned())]
    );
}

#[test]
fn test_open_existing_requires_database_file() {
    let dir = tempfile::tempdir().unwrap();
    assert!(MetaStore::open_existing(dir.path().join("missing.redb")).is_err());
}

#[test]
fn test_count_and_delete_project_cache_purge() {
    let (_dir, store) = store();
    let file_digests = vec!["a".repeat(64)];
    let metadata_digests = vec!["b".repeat(64)];
    store
        .put_mirror_page(
            "pypi/flask",
            &record(),
            "pypi",
            "flask",
            "Flask",
            "pypi",
            Some("archived"),
            Some("read only"),
            &[(
                file_digests[0].clone(),
                "https://files.example/flask.whl".to_owned(),
                Some(123),
            )],
            &[(
                metadata_digests[0].clone(),
                "https://files.example/flask.whl.metadata".to_owned(),
                "c".repeat(64),
            )],
        )
        .unwrap();
    assert_eq!(
        store.get_project_status("pypi", "flask").unwrap().unwrap(),
        crate::meta::ProjectStatusRecord {
            status: Some("archived".to_owned()),
            reason: Some("read only".to_owned()),
        }
    );

    assert_eq!(
        store
            .count_project_cache_purge("pypi", "flask", &file_digests, &metadata_digests)
            .unwrap(),
        crate::meta::ProjectCachePurgeCounts {
            index_pages: 1,
            project_records: 1,
            project_status_records: 1,
            file_url_records: 1,
            metadata_records: 1,
        }
    );
    assert_eq!(
        store
            .delete_project_cache("pypi", "flask", &file_digests, &metadata_digests)
            .unwrap(),
        crate::meta::ProjectCachePurgeCounts {
            index_pages: 1,
            project_records: 1,
            project_status_records: 1,
            file_url_records: 1,
            metadata_records: 1,
        }
    );
    assert!(store.get_index("pypi/flask").unwrap().is_none());
    assert!(store.get_file_url("a".repeat(64).as_str()).unwrap().is_none());
    assert!(store.get_metadata("b".repeat(64).as_str()).unwrap().is_none());
    assert!(store.get_project_status("pypi", "flask").unwrap().is_none());
    assert!(store.list_projects("pypi").unwrap().is_empty());
}
