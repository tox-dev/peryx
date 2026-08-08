//! The neutral pending-operations-health view an operator surface renders.
//!
//! An administrator needs to see how many admitted writes are still in flight, how many finalized, and
//! how many gave up or expired, without paging every operation by hand or reading tenant data.
//! [`OperationsView`] carries the aggregate every viewer reads and, for an administrator, a bounded page
//! of per-operation rows. The models are pure serde with no I/O, so the same type crosses the
//! server/browser boundary the placement view already established.
//!
//! The aggregate is whole-ledger; the rows are one capped page in operation-id order with a cursor to
//! resume, so a large ledger never turns a render into an unbounded payload.

use serde::{Deserialize, Serialize};

use crate::view::UiOperationStatus;

/// How admitted writes split across their client-facing status, plus the total they sum to. Four counts
/// and a total regardless of ledger size, so the summary never scales with the operation count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OperationsHealth {
    /// Writes admitted and in flight within their retention deadline.
    pub pending: u64,
    /// Writes finalized at the home.
    pub published: u64,
    /// Writes that gave up before finalizing.
    pub failed: u64,
    /// Writes that never finalized and outlived their retention deadline.
    pub expired: u64,
    /// The sum of the four states, so a reader sees the ledger size the split covers.
    pub total: u64,
}

/// One admitted write's row: its operation id, its client-facing status, when its record last changed,
/// and when it may be pruned.
///
/// Carries no response bytes, tenant identity, or repository coordinate, so an administrator inspects a
/// write's convergence by operation id without the row leaking what it wrote or who owns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationRow {
    pub operation: String,
    pub status: UiOperationStatus,
    /// Unix seconds when this record last changed, so a stale write shows as age.
    pub updated_at: i64,
    /// Unix seconds when the record may be pruned, present only when a retention deadline is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

/// The operations-health view filtered to the caller's class.
///
/// Every admitted caller reads the aggregate and the observation time; only an administrator reads the
/// per-operation `rows` and the `next_cursor` to page them. A withheld page is absent rather than empty,
/// so an operator cannot mistake a filtered view for a settled ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationsView {
    /// Unix seconds when the view was taken, so a stale render shows as age rather than health.
    pub captured_at: i64,
    pub health: OperationsHealth,
    /// A bounded page of per-operation rows in operation-id order, present only for an administrator.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<Vec<OperationRow>>,
    /// The operation id to resume the page after, present only with `rows` and only when more remain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

#[cfg(test)]
#[path = "../tests/unit/operations/tests.rs"]
mod tests;
