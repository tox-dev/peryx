use std::error::Error as _;

use super::{record, store};
use crate::meta::{CachedIndex, FileSource, MetaStore};

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
fn test_put_cached_page_records_file_url_size() {
    let (_dir, store) = store();
    store
        .put_cached_page(
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
    let table: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("index_document");
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
