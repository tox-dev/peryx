//! Regular operation tracing swallows ledger faults. Durability-gated writes use a pending record as a
//! crash-safe metadata checkpoint and surface ledger errors.

use peryx_storage::meta::{OperationClaim, OperationOutcomeRecord, OperationResult};

use super::ServingState;

/// How long a finalized write's recorded outcome is retained before the reaper prunes it. Bounded apart
/// from the background retry lifetime while covering a client's realistic retry window, so a write that
/// never finalizes reads `expired` and a terminal record is pruned once its deadline passes.
const OPERATION_RETENTION_SECS: i64 = 24 * 3600;

impl ServingState {
    /// Claims an operation without a pending checkpoint. A terminal record starts a new attempt under the
    /// same ID.
    ///
    /// # Errors
    /// Returns a metadata error when the operation record cannot be read or updated.
    pub fn begin_retryable_write(
        &self,
        operation: &str,
    ) -> Result<Option<OperationOutcomeRecord>, peryx_storage::meta::MetaError> {
        let now = (self.clock)();
        let expiry = Some(now + OPERATION_RETENTION_SECS);
        match self.meta.claim_operation(operation, expiry, now)? {
            OperationClaim::Admitted => Ok(None),
            OperationClaim::Existing(record) if !record.state.is_terminal() => Ok(Some(record)),
            OperationClaim::Existing(_) => {
                self.meta.restart_operation(operation, expiry, now)?;
                Ok(None)
            }
        }
    }

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
