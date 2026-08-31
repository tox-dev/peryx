use crate::meta::MetaStore;

mod analytics_tests;
mod blob_placement_scan_tests;
mod blob_placement_tests;
mod bootstrap_tests;
mod copy_cursor_tests;
mod driver_txn_tests;
mod error_tests;
mod external_identity_tests;
mod finalize_tests;
mod ingress_intent_tests;
mod integration_tests;
mod job_tests;
mod journal_tests;
mod operation_outcome_tests;
mod placement_tests;
mod policy_decision_tests;
mod quota_tests;
mod reclaim_guard_tests;
mod reclamation_cursor_tests;
mod reclamation_scan_tests;
mod reclamation_tests;
mod replica_tests;
mod repository_tests;
mod revocation_journal_tests;
mod revocation_scan_tests;
mod revocation_tests;
mod role_grant_tests;
mod scoped_token_tests;
mod transfer_audit_tests;
mod user_tests;
mod webhook_tests;
mod writer_tests;

pub(super) fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, store)
}

pub(super) fn distributed_store() -> (tempfile::TempDir, MetaStore) {
    let (dir, store) = store();
    store.initialize_distributed_state().unwrap();
    (dir, store)
}
