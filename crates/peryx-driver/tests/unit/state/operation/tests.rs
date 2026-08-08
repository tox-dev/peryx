use std::sync::Arc;

use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::{MetaStore, OperationState};

use super::*;
use crate::state::AppState;

const NOW: i64 = 1_000;

fn state() -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let state = AppState::with_clock(meta, blobs, 60, Vec::new(), Arc::new(|| NOW));
    (dir, state)
}

#[test]
fn test_claim_admitted_write_records_a_pending_write_with_a_retention_deadline() {
    let (_dir, state) = state();
    state.claim_admitted_write("op");
    let stored = state.meta.operation_outcome("op").unwrap().unwrap();
    assert_eq!(stored.state, OperationState::Pending);
    assert_eq!(stored.expiry_unix, Some(NOW + OPERATION_RETENTION_SECS));
}

#[test]
fn test_claim_admitted_write_that_never_finalizes_expires() {
    let (_dir, state) = state();
    state.claim_admitted_write("op");
    let health = state
        .meta
        .operation_outcome_health(NOW + OPERATION_RETENTION_SECS)
        .unwrap();
    assert_eq!(
        health.expired, 1,
        "an unfinalized write reads expired past its deadline"
    );
}

#[test]
fn test_finalize_admitted_write_stamps_the_terminal_result() {
    let (_dir, state) = state();
    state.claim_admitted_write("op");
    state.finalize_admitted_write("op", OperationResult::Published, b"serial-7");
    let stored = state.meta.operation_outcome("op").unwrap().unwrap();
    assert_eq!(stored.state, OperationState::Published);
    assert_eq!(stored.response, b"serial-7");
}

#[test]
fn test_finalize_admitted_write_records_a_terminal_failure() {
    let (_dir, state) = state();
    state.claim_admitted_write("op");
    state.finalize_admitted_write("op", OperationResult::Failed, b"");
    assert_eq!(
        state.meta.operation_outcome("op").unwrap().unwrap().state,
        OperationState::Failed
    );
}

#[test]
fn test_a_retry_leaves_the_terminal_record_unchanged() {
    let (_dir, state) = state();
    state.claim_admitted_write("op");
    state.finalize_admitted_write("op", OperationResult::Published, b"serial-7");
    // A retry re-claims and re-finalizes the same id; neither disturbs the terminal record.
    state.claim_admitted_write("op");
    state.finalize_admitted_write("op", OperationResult::Failed, b"clobber");
    let stored = state.meta.operation_outcome("op").unwrap().unwrap();
    assert_eq!(stored.state, OperationState::Published);
    assert_eq!(stored.response, b"serial-7");
}
