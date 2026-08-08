//! A durable ledger of admitted-write outcomes keyed by operation id, so a retry of the same operation
//! replays its original result instead of running a second mutation.
//!
//! A write admitted at one node can outlive the request that opened it and be retried. Claiming the
//! operation id records it as pending before the mutation runs; finalizing stamps the terminal result
//! and the response bytes a retry replays. A retry re-claims the same id, finds the existing record, and
//! replays it - pending while the first attempt is still in flight, or the finalized response once it
//! committed - so the mutation runs once. This module owns the persistence; deriving the client-facing
//! status and scoping the id to an authority live above it.

use std::ops::Bound::{Excluded, Unbounded};

use redb::ReadableTable as _;
use serde::{Deserialize, Serialize};

use super::{MetaError, MetaStore, OPERATION_OUTCOME};

/// The largest page an operation listing returns, so one query never scans the whole ledger.
const MAX_QUERY_LIMIT: usize = 100;

/// The lifecycle of one admitted write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    /// Admitted and in flight; no terminal result yet.
    Pending,
    /// Finalized at the home. Terminal.
    Published,
    /// Gave up before finalizing. Terminal.
    Failed,
}

impl OperationState {
    /// Whether the write reached a terminal result and will not change again.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Published | Self::Failed)
    }
}

/// The terminal result a caller finalizes an operation to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationResult {
    Published,
    Failed,
}

impl OperationResult {
    const fn state(self) -> OperationState {
        match self {
            Self::Published => OperationState::Published,
            Self::Failed => OperationState::Failed,
        }
    }
}

/// The durable record of one admitted write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationOutcomeRecord {
    pub state: OperationState,
    /// The opaque response the ecosystem finalized, replayed verbatim to a retry. Empty while pending.
    pub response: Vec<u8>,
    /// When the record may be pruned, or `None` for no retention bound.
    pub expiry_unix: Option<i64>,
    pub updated_at_unix: i64,
}

/// The effect of claiming an operation id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationClaim {
    /// The id was free; it is now admitted as pending and this caller owns the single mutation.
    Admitted,
    /// The id was already claimed; here is its record, so a retry replays it rather than remutating.
    Existing(OperationOutcomeRecord),
}

/// A rejected finalize.
#[derive(Debug, thiserror::Error)]
pub enum OperationOutcomeError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("operation {operation} was never admitted")]
    NotAdmitted { operation: String },
    #[error("operation {operation} is already finalized")]
    AlreadyFinal { operation: String },
}

/// One row of an operation listing: the operation id and the durable fields a health view reads.
///
/// A view derives the client-facing status, age, and retention from these fields. The row carries no
/// response bytes and no tenant coordinate, so it exposes a write's convergence without leaking what it
/// wrote or who owns it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OperationOutcomeRow {
    pub operation: String,
    pub state: OperationState,
    /// When the record may be pruned, from which an expired status is derived against a clock.
    pub expiry_unix: Option<i64>,
    pub updated_at_unix: i64,
}

/// A bounded, cursor-paginated query over operation outcomes in operation-id order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationOutcomeQuery {
    /// Resume after this operation id, exclusive; `None` starts at the first.
    pub cursor: Option<String>,
    pub limit: usize,
}

impl Default for OperationOutcomeQuery {
    fn default() -> Self {
        Self {
            cursor: None,
            limit: 25,
        }
    }
}

/// One bounded page of operation rows in operation-id order, with a cursor to resume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OperationOutcomePage {
    pub rows: Vec<OperationOutcomeRow>,
    /// The operation id to resume after, present only when more rows remain past this page.
    pub next_cursor: Option<String>,
}

/// The counts an operations-health view shows, bucketed by the client-facing status a write reads.
///
/// The status is derived at the query's clock. Four numbers regardless of ledger size, so the summary
/// never scales with the writes it covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct OperationOutcomeHealth {
    pub pending: u64,
    pub published: u64,
    pub failed: u64,
    pub expired: u64,
}

impl OperationOutcomeHealth {
    /// The total operations across the four states.
    #[must_use]
    pub const fn total(self) -> u64 {
        self.pending + self.published + self.failed + self.expired
    }
}

/// A rejected operation list.
#[derive(Debug, thiserror::Error)]
pub enum OperationOutcomeQueryError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("limit must be between 1 and {MAX_QUERY_LIMIT}")]
    InvalidLimit,
}

impl MetaStore {
    /// Claim `operation` as an admitted, pending write, or return its existing record when a prior
    /// attempt already claimed it. The read and the insert share one write transaction, so two racing
    /// attempts never both admit the same id.
    ///
    /// # Errors
    /// Returns a store error when the row cannot be read, encoded, or committed.
    pub fn claim_operation(
        &self,
        operation: &str,
        expiry_unix: Option<i64>,
        now: i64,
    ) -> Result<OperationClaim, MetaError> {
        let txn = self.db.begin_write()?;
        let existing = {
            let table = txn.open_table(OPERATION_OUTCOME)?;
            table
                .get(operation)?
                .map(|value| serde_json::from_slice::<OperationOutcomeRecord>(value.value()))
                .transpose()?
        };
        if let Some(record) = existing {
            return Ok(OperationClaim::Existing(record));
        }
        let record = OperationOutcomeRecord {
            state: OperationState::Pending,
            response: Vec::new(),
            expiry_unix,
            updated_at_unix: now,
        };
        let value = serde_json::to_vec(&record)?;
        txn.open_table(OPERATION_OUTCOME)?.insert(operation, value.as_slice())?;
        txn.commit()?;
        Ok(OperationClaim::Admitted)
    }

    /// Stamp `operation`'s terminal `result` and the `response` a retry replays.
    ///
    /// # Errors
    /// Returns [`OperationOutcomeError::NotAdmitted`] when the id was never claimed,
    /// [`OperationOutcomeError::AlreadyFinal`] when it already holds a terminal result, or a store error
    /// when the row cannot be read, encoded, or committed.
    pub fn finalize_operation(
        &self,
        operation: &str,
        result: OperationResult,
        response: &[u8],
        now: i64,
    ) -> Result<OperationOutcomeRecord, OperationOutcomeError> {
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        let existing = {
            let table = txn.open_table(OPERATION_OUTCOME).map_err(MetaError::from)?;
            table
                .get(operation)
                .map_err(MetaError::from)?
                .map(|value| serde_json::from_slice::<OperationOutcomeRecord>(value.value()))
                .transpose()
                .map_err(MetaError::from)?
        };
        let Some(record) = existing else {
            return Err(OperationOutcomeError::NotAdmitted {
                operation: operation.to_owned(),
            });
        };
        if record.state.is_terminal() {
            return Err(OperationOutcomeError::AlreadyFinal {
                operation: operation.to_owned(),
            });
        }
        let finalized = OperationOutcomeRecord {
            state: result.state(),
            response: response.to_vec(),
            expiry_unix: record.expiry_unix,
            updated_at_unix: now,
        };
        let value = serde_json::to_vec(&finalized).map_err(MetaError::from)?;
        txn.open_table(OPERATION_OUTCOME)
            .map_err(MetaError::from)?
            .insert(operation, value.as_slice())
            .map_err(MetaError::from)?;
        txn.commit().map_err(MetaError::from)?;
        Ok(finalized)
    }

    /// Read one operation's record, or `None` when the id was never claimed.
    ///
    /// # Errors
    /// Returns a store error when the row cannot be read or decoded.
    pub fn operation_outcome(&self, operation: &str) -> Result<Option<OperationOutcomeRecord>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(OPERATION_OUTCOME)?;
        Ok(table
            .get(operation)?
            .map(|value| serde_json::from_slice(value.value()))
            .transpose()?)
    }

    /// List operation outcomes in operation-id order with an exclusive cursor, giving an operations-health
    /// view a bounded data source of per-operation rows.
    ///
    /// Reads one row past `limit` to decide whether more remain: a full page carries a `next_cursor`
    /// pointing at its last operation id, and a page that reaches the end carries none. The read span is
    /// bounded by the validated limit, so a large ledger never turns into one unbounded scan. Rows carry
    /// the durable fields only; the client-facing status is derived above the store against a clock.
    ///
    /// # Errors
    /// Returns [`OperationOutcomeQueryError::InvalidLimit`] for a limit outside `1..=MAX_QUERY_LIMIT`, or a
    /// store error when a row cannot be read or decoded.
    pub fn list_operation_outcomes(
        &self,
        query: &OperationOutcomeQuery,
    ) -> Result<OperationOutcomePage, OperationOutcomeQueryError> {
        if !(1..=MAX_QUERY_LIMIT).contains(&query.limit) {
            return Err(OperationOutcomeQueryError::InvalidLimit);
        }
        let txn = self.db.begin_read().map_err(MetaError::from)?;
        let table = txn.open_table(OPERATION_OUTCOME).map_err(MetaError::from)?;
        let entries = query
            .cursor
            .as_ref()
            .map_or_else(
                || table.iter(),
                |cursor| table.range::<&str>((Excluded(cursor.as_str()), Unbounded)),
            )
            .map_err(MetaError::from)?;
        let mut rows = Vec::with_capacity(query.limit + 1);
        for entry in entries {
            let (key, value) = entry.map_err(MetaError::from)?;
            let record: OperationOutcomeRecord = serde_json::from_slice(value.value()).map_err(MetaError::from)?;
            rows.push(OperationOutcomeRow {
                operation: key.value().to_owned(),
                state: record.state,
                expiry_unix: record.expiry_unix,
                updated_at_unix: record.updated_at_unix,
            });
            if rows.len() > query.limit {
                break;
            }
        }
        let next_cursor = (rows.len() > query.limit).then(|| rows[query.limit - 1].operation.clone());
        rows.truncate(query.limit);
        Ok(OperationOutcomePage { rows, next_cursor })
    }

    /// Bucket every operation by the client-facing status it reads at `now` in one pass, giving an
    /// operations-health view its aggregate without paging the whole ledger itself.
    ///
    /// A published or failed record is terminal; a pending record whose retention deadline has passed at
    /// `now` reads as expired, and one still within its deadline as pending. The output is four counts
    /// regardless of ledger size, so the summary aggregates before serialization.
    ///
    /// # Errors
    /// Returns a store error if a row cannot be read or decoded.
    pub fn operation_outcome_health(&self, now: i64) -> Result<OperationOutcomeHealth, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(OPERATION_OUTCOME)?;
        let mut health = OperationOutcomeHealth::default();
        for entry in table.iter()? {
            let (_key, value) = entry?;
            let record: OperationOutcomeRecord = serde_json::from_slice(value.value())?;
            match record.state {
                OperationState::Published => health.published += 1,
                OperationState::Failed => health.failed += 1,
                OperationState::Pending if record.expiry_unix.is_some_and(|expiry| now >= expiry) => {
                    health.expired += 1;
                }
                OperationState::Pending => health.pending += 1,
            }
        }
        Ok(health)
    }

    /// Remove up to `limit` terminal records whose retention deadline has passed at `now`, returning how
    /// many were pruned. A pending record is never pruned, since its write may still finalize.
    ///
    /// # Errors
    /// Returns a store error when a row cannot be read or the delete cannot be committed.
    pub fn prune_operation_outcomes(&self, now: i64, limit: usize) -> Result<usize, MetaError> {
        let txn = self.db.begin_write()?;
        let pruned;
        {
            let mut table = txn.open_table(OPERATION_OUTCOME)?;
            let mut doomed = Vec::new();
            for entry in table.iter()? {
                if doomed.len() >= limit {
                    break;
                }
                let (key, value) = entry?;
                let record: OperationOutcomeRecord = serde_json::from_slice(value.value())?;
                if record.state.is_terminal() && record.expiry_unix.is_some_and(|expiry| now >= expiry) {
                    doomed.push(key.value().to_owned());
                }
            }
            for key in &doomed {
                table.remove(key.as_str())?;
            }
            pruned = doomed.len();
        }
        txn.commit()?;
        Ok(pruned)
    }
}

#[cfg(test)]
#[path = "../../tests/unit/meta/operation_outcome_fault_tests.rs"]
mod fault_tests;
