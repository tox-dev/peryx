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
    /// retention deadline `OPERATION_RETENTION_SECS` out so a write that never finalizes reads `expired`
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
#[path = "../../tests/unit/state/operation/tests.rs"]
mod tests;
