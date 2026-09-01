//! Claiming an operation ID before mutation lets retries replay the pending or terminal record instead of
//! mutating twice. Callers scope IDs and derive client-facing status.

use std::ops::Bound::{Excluded, Unbounded};

use redb::ReadableTable as _;
use serde::{Deserialize, Serialize};

use super::{MetaError, MetaStore, OPERATION_OUTCOME};

/// Bounds one operation-ledger scan.
const MAX_QUERY_LIMIT: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    Pending,
    /// Terminal success at the home.
    Published,
    /// Terminal failure before finalization.
    Failed,
}

impl OperationState {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Published | Self::Failed)
    }
}

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationOutcomeRecord {
    pub state: OperationState,
    /// Opaque replay response; empty while pending.
    pub response: Vec<u8>,
    /// `None` retains the record without a time bound.
    pub expiry_unix: Option<i64>,
    pub updated_at_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationClaim {
    /// This caller owns the mutation.
    Admitted,
    /// The caller must replay this record.
    Existing(OperationOutcomeRecord),
}

#[derive(Debug, thiserror::Error)]
pub enum OperationOutcomeError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("operation {operation} was never admitted")]
    NotAdmitted { operation: String },
    #[error("operation {operation} is already finalized")]
    AlreadyFinal { operation: String },
}

/// Excludes response bytes and tenant coordinates from health views.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OperationOutcomeRow {
    pub operation: String,
    pub state: OperationState,
    pub expiry_unix: Option<i64>,
    pub updated_at_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationOutcomeQuery {
    /// Exclusive; `None` starts at the first ID.
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OperationOutcomePage {
    pub rows: Vec<OperationOutcomeRow>,
    /// Present only when another page remains.
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct OperationOutcomeHealth {
    pub pending: u64,
    pub published: u64,
    pub failed: u64,
    pub expired: u64,
}

impl OperationOutcomeHealth {
    #[must_use]
    pub const fn total(self) -> u64 {
        self.pending + self.published + self.failed + self.expired
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OperationOutcomeQueryError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("limit must be between 1 and {MAX_QUERY_LIMIT}")]
    InvalidLimit,
}

impl MetaStore {
    /// Atomically admits an unclaimed ID or returns its record, preventing racing attempts from both
    /// owning the mutation.
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

    /// Starts another attempt under a terminal operation ID. A pending operation retains its checkpoint.
    ///
    /// # Errors
    /// Returns a store error when the row cannot be read, encoded, or committed.
    pub fn restart_operation(
        &self,
        operation: &str,
        expiry_unix: Option<i64>,
        now: i64,
    ) -> Result<OperationOutcomeRecord, MetaError> {
        let txn = self.db.begin_write()?;
        let mut outcomes = txn.open_table(OPERATION_OUTCOME)?;
        let existing = outcomes
            .get(operation)?
            .map(|value| serde_json::from_slice::<OperationOutcomeRecord>(value.value()))
            .transpose()?;
        if let Some(record) = existing.filter(|record| !record.state.is_terminal()) {
            return Ok(record);
        }
        let record = OperationOutcomeRecord {
            state: OperationState::Pending,
            response: Vec::new(),
            expiry_unix,
            updated_at_unix: now,
        };
        let encoded = serde_json::to_vec(&record)?;
        outcomes.insert(operation, encoded.as_slice())?;
        drop(outcomes);
        txn.commit()?;
        Ok(record)
    }

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

    /// Returns `None` when the ID was never claimed.
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

    /// Returns ID-ordered rows after an exclusive cursor. Reads one extra row to decide whether to return
    /// `next_cursor`; callers derive status against their clock.
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

    /// Counts published, failed, pending, and expired operations at `now` without serializing the ledger.
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

    /// Removes up to `limit` expired terminal records. Pending records remain eligible to finalize.
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

pub(super) fn checkpoint_pending_operation(
    txn: &redb::WriteTransaction,
    operation: &str,
    response: &[u8],
    now: i64,
) -> Result<(), OperationOutcomeError> {
    let mut outcomes = txn.open_table(OPERATION_OUTCOME).map_err(MetaError::from)?;
    let Some(mut record) = outcomes
        .get(operation)
        .map_err(MetaError::from)?
        .map(|value| serde_json::from_slice::<OperationOutcomeRecord>(value.value()))
        .transpose()
        .map_err(MetaError::from)?
    else {
        return Err(OperationOutcomeError::NotAdmitted {
            operation: operation.to_owned(),
        });
    };
    if record.state.is_terminal() {
        return Err(OperationOutcomeError::AlreadyFinal {
            operation: operation.to_owned(),
        });
    }
    record.response = response.to_vec();
    record.updated_at_unix = now;
    let encoded = serde_json::to_vec(&record).map_err(MetaError::from)?;
    outcomes
        .insert(operation, encoded.as_slice())
        .map_err(MetaError::from)?;
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/meta/operation_outcome_fault_tests.rs"]
mod fault_tests;
