use crate::meta::{CachedIndex, MetaStore};

mod error_tests;
mod files_tests;
mod index_tests;
mod integration_tests;
mod journal_tests;
mod projects_tests;
mod record_tests;
mod summary_tests;
mod uploads_tests;
mod webhook_tests;

pub(super) fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    (dir, store)
}

pub(super) fn record() -> CachedIndex {
    CachedIndex {
        etag: Some("\"abc\"".to_owned()),
        last_serial: Some(42),
        fetched_at_unix: 1_700_000_000,
        content_type: None,

        fresh_secs: None,
        body: b"<html></html>".to_vec(),
    }
}
