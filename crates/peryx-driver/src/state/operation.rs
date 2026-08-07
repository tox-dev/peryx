//! Recording an admitted write's outcome in the durable operation-outcome ledger from the write path.
//!
//! An ecosystem mutation claims its operation id as pending before it runs, then finalizes the terminal
//! result once the bytes commit. The ledger records the write's convergence for the pending-operations
//! view and dedups a retry to a single terminal record. Recording is best effort and off the publish's
//! critical path: a ledger fault is swallowed rather than turned into a client error, so it never fails a
//! durable publish, and the response bytes never reach a log. The ledger only records - it never gates
//! the mutation, so a repeated write always re-commits its content-addressed bytes and stays retrievable
//! rather than short-circuiting on the recorded claim.

use peryx_storage::meta::OperationResult;

use super::ServingState;

/// How long a finalized write's recorded outcome is retained before the reaper prunes it. Bounded apart
/// from the background retry lifetime while covering a client's realistic retry window, so a write that
/// never finalizes reads `expired` and a terminal record is pruned once its deadline passes.
const OPERATION_RETENTION_SECS: i64 = 24 * 3600;

impl ServingState {
    /// Claim `operation` as an admitted, pending write before its mutation runs, recording it with a
    /// retention deadline [`OPERATION_RETENTION_SECS`] out so a write that never finalizes reads `expired`
    /// and is pruned once the deadline passes. A retry of the same id finds the existing record rather than
    /// inserting a second.
    ///
    /// Best effort and off the publish's critical path: the claim records the write's admission and never
    /// gates it, so a ledger fault is swallowed and the caller runs the mutation regardless. Persistence
    /// never depends on this claim.
    pub fn claim_admitted_write(&self, operation: &str) {
        let now = (self.clock)();
        let _ = self
            .meta
            .claim_operation(operation, Some(now + OPERATION_RETENTION_SECS), now);
    }

    /// Finalize `operation` to its terminal `result`, retaining `response` a health view keys the recorded
    /// outcome on. Best effort: a rejected finalize is swallowed, leaving the record pending for a retry to
    /// re-drive, and a retry that re-finalizes an already-terminal record leaves it unchanged. The response
    /// bytes never reach a log.
    pub fn finalize_admitted_write(&self, operation: &str, result: OperationResult, response: &[u8]) {
        let _ = self
            .meta
            .finalize_operation(operation, result, response, (self.clock)());
    }
}

#[cfg(test)]
mod tests {
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
}
