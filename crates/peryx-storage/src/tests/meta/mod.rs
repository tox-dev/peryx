use crate::meta::MetaStore;

mod analytics_tests;
mod driver_txn_tests;
mod error_tests;
mod integration_tests;
mod job_tests;
mod journal_tests;
mod replica_tests;
mod webhook_tests;

pub(super) fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, store)
}
