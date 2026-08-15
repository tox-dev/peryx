use std::ops::Bound::{Excluded, Unbounded};

use peryx_identity::{ArtifactDigest, RevocationReason, UserId};
use redb::ReadableTable as _;
use serde::{Deserialize, Serialize};

use super::{DIGEST_REVOCATION, DIGEST_REVOCATION_STATE, MetaError, MetaStore};

const MAX_QUERY_LIMIT: usize = 100;
const ACTIVE_COUNT_KEY: &str = "active_count";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum DigestRevocationState {
    Active,
    Lifted { lifted_by: UserId, lifted_at_unix: i64 },
}

impl DigestRevocationState {
    #[must_use]
    pub const fn status(&self) -> DigestRevocationStatus {
        match self {
            Self::Active => DigestRevocationStatus::Active,
            Self::Lifted { .. } => DigestRevocationStatus::Lifted,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DigestRevocationStatus {
    Active,
    Lifted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DigestRevocation {
    pub digest: ArtifactDigest,
    pub reason: RevocationReason,
    pub created_by: UserId,
    pub created_at_unix: i64,
    pub state: DigestRevocationState,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PutRevocationOutcome {
    Created(DigestRevocation),
    Unchanged(DigestRevocation),
    Reopened(DigestRevocation),
}

impl PutRevocationOutcome {
    #[must_use]
    pub const fn record(&self) -> &DigestRevocation {
        match self {
            Self::Created(record) | Self::Unchanged(record) | Self::Reopened(record) => record,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PutRevocationError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("the digest is already revoked for a different reason")]
    ReasonConflict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiftRevocationOutcome {
    Lifted(DigestRevocation),
    Unchanged(DigestRevocation),
}

impl LiftRevocationOutcome {
    #[must_use]
    pub const fn record(&self) -> &DigestRevocation {
        match self {
            Self::Lifted(record) | Self::Unchanged(record) => record,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestRevocationQuery {
    pub status: Option<DigestRevocationStatus>,
    pub cursor: Option<ArtifactDigest>,
    pub limit: usize,
}

impl Default for DigestRevocationQuery {
    fn default() -> Self {
        Self {
            status: None,
            cursor: None,
            limit: 25,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DigestRevocationPage {
    pub revocations: Vec<DigestRevocation>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum DigestRevocationQueryError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("limit must be between 1 and {MAX_QUERY_LIMIT}")]
    InvalidLimit,
}

/// Rebuilds the derived count for legacy or drifted stores. Corrupt rows remain queryable as errors but
/// do not prevent startup.
///
/// # Errors
/// Returns a store error when the tables cannot be opened, read, or written.
pub(super) fn backfill_digest_revocation_state(txn: &redb::WriteTransaction) -> Result<(), MetaError> {
    let active = {
        let records = txn.open_table(DIGEST_REVOCATION)?;
        let mut active: u64 = 0;
        for entry in records.iter()? {
            let (_key, value) = entry?;
            // The read path reports corrupt rows; startup can rebuild the count without classifying them.
            let Ok(record) = serde_json::from_slice::<DigestRevocation>(value.value()) else {
                continue;
            };
            if record.state == DigestRevocationState::Active {
                active += 1;
            }
        }
        active
    };
    let mut state = txn.open_table(DIGEST_REVOCATION_STATE)?;
    if state.get(ACTIVE_COUNT_KEY)?.map_or(0, |count| count.value()) != active {
        state.insert(ACTIVE_COUNT_KEY, active)?;
    }
    Ok(())
}

impl MetaStore {
    /// # Errors
    /// Returns a conflict when an active row has another reason, or a store error when the row cannot
    /// be read, encoded, or committed.
    pub fn put_digest_revocation(
        &self,
        digest: &ArtifactDigest,
        reason: &RevocationReason,
        actor: &UserId,
        now: i64,
    ) -> Result<PutRevocationOutcome, PutRevocationError> {
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        let key = digest.canonical();
        let existing = {
            let table = txn.open_table(DIGEST_REVOCATION).map_err(MetaError::from)?;
            table
                .get(key.as_str())
                .map_err(MetaError::from)?
                .map(|value| serde_json::from_slice::<DigestRevocation>(value.value()))
                .transpose()
                .map_err(MetaError::from)?
        };
        let (record, outcome) = match existing {
            Some(record) if record.state == DigestRevocationState::Active && record.reason == *reason => {
                return Ok(PutRevocationOutcome::Unchanged(record));
            }
            Some(record) if record.state == DigestRevocationState::Active => {
                return Err(PutRevocationError::ReasonConflict);
            }
            Some(record) => {
                let reopened = DigestRevocation {
                    digest: digest.clone(),
                    reason: reason.clone(),
                    created_by: actor.clone(),
                    created_at_unix: now,
                    state: DigestRevocationState::Active,
                    revision: record.revision + 1,
                };
                (reopened.clone(), PutRevocationOutcome::Reopened(reopened))
            }
            None => {
                let created = DigestRevocation {
                    digest: digest.clone(),
                    reason: reason.clone(),
                    created_by: actor.clone(),
                    created_at_unix: now,
                    state: DigestRevocationState::Active,
                    revision: 1,
                };
                (created.clone(), PutRevocationOutcome::Created(created))
            }
        };
        let encoded = serde_json::to_vec(&record).map_err(MetaError::from)?;
        txn.open_table(DIGEST_REVOCATION)
            .map_err(MetaError::from)?
            .insert(key.as_str(), encoded.as_slice())
            .map_err(MetaError::from)?;
        let active_count = txn
            .open_table(DIGEST_REVOCATION_STATE)
            .map_err(MetaError::from)?
            .get(ACTIVE_COUNT_KEY)
            .map_err(MetaError::from)?
            .map_or(0, |count| count.value())
            .checked_add(1)
            .ok_or_else(|| MetaError::DriverPrecondition("digest revocation active count overflow".to_owned()))?;
        txn.open_table(DIGEST_REVOCATION_STATE)
            .map_err(MetaError::from)?
            .insert(ACTIVE_COUNT_KEY, active_count)
            .map_err(MetaError::from)?;
        txn.commit().map_err(MetaError::from)?;
        Ok(outcome)
    }

    /// Retains creation evidence after lifting an active revocation.
    ///
    /// # Errors
    /// Returns a store error when the row cannot be read, encoded, or committed.
    pub fn lift_digest_revocation(
        &self,
        digest: &ArtifactDigest,
        actor: &UserId,
        now: i64,
    ) -> Result<Option<LiftRevocationOutcome>, MetaError> {
        let txn = self.db.begin_write()?;
        let key = digest.canonical();
        let existing = {
            let table = txn.open_table(DIGEST_REVOCATION)?;
            table
                .get(key.as_str())?
                .map(|value| serde_json::from_slice::<DigestRevocation>(value.value()))
                .transpose()?
        };
        let Some(mut record) = existing else {
            return Ok(None);
        };
        if matches!(record.state, DigestRevocationState::Lifted { .. }) {
            return Ok(Some(LiftRevocationOutcome::Unchanged(record)));
        }
        record.state = DigestRevocationState::Lifted {
            lifted_by: actor.clone(),
            lifted_at_unix: now,
        };
        record.revision += 1;
        let encoded = serde_json::to_vec(&record)?;
        txn.open_table(DIGEST_REVOCATION)?
            .insert(key.as_str(), encoded.as_slice())?;
        let active_count = txn
            .open_table(DIGEST_REVOCATION_STATE)?
            .get(ACTIVE_COUNT_KEY)?
            .map_or(0, |count| count.value())
            .checked_sub(1)
            .ok_or_else(|| MetaError::DriverPrecondition("digest revocation active count underflow".to_owned()))?;
        txn.open_table(DIGEST_REVOCATION_STATE)?
            .insert(ACTIVE_COUNT_KEY, active_count)?;
        txn.commit()?;
        Ok(Some(LiftRevocationOutcome::Lifted(record)))
    }

    /// Uses the transactional count without decoding revocation rows.
    ///
    /// # Errors
    /// Returns a store error when the revocation index cannot be validated or read.
    pub fn has_active_digest_revocation(&self) -> Result<bool, MetaError> {
        let txn = self.db.begin_read()?;
        let records_table_exists = match txn.open_table(DIGEST_REVOCATION) {
            Ok(_) => true,
            Err(redb::TableError::TableDoesNotExist(_)) => false,
            Err(error) => return Err(error.into()),
        };
        let state_table = match txn.open_table(DIGEST_REVOCATION_STATE) {
            Ok(table) => Some(table),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(error) => return Err(error.into()),
        };
        match (records_table_exists, state_table) {
            (false, None) => Ok(false),
            (true, Some(table)) => Ok(table.get(ACTIVE_COUNT_KEY)?.is_some_and(|count| count.value() > 0)),
            _ => Err(MetaError::DriverPrecondition(
                "digest revocation index is incomplete".to_owned(),
            )),
        }
    }

    /// # Errors
    /// Returns a store error when the row cannot be read or decoded.
    pub fn digest_revocation(&self, digest: &ArtifactDigest) -> Result<Option<DigestRevocation>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(DIGEST_REVOCATION) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(table
            .get(digest.canonical().as_str())?
            .map(|value| serde_json::from_slice(value.value()))
            .transpose()?)
    }

    /// Returns rows in canonical digest order after an exclusive cursor.
    ///
    /// # Errors
    /// Returns a validation error for an out-of-range limit, or a store error when rows cannot be read
    /// or decoded.
    pub fn query_digest_revocations(
        &self,
        query: &DigestRevocationQuery,
    ) -> Result<DigestRevocationPage, DigestRevocationQueryError> {
        if !(1..=MAX_QUERY_LIMIT).contains(&query.limit) {
            return Err(DigestRevocationQueryError::InvalidLimit);
        }
        let txn = self.db.begin_read().map_err(MetaError::from)?;
        let table = match txn.open_table(DIGEST_REVOCATION) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Ok(DigestRevocationPage {
                    revocations: Vec::new(),
                    next_cursor: None,
                });
            }
            Err(error) => return Err(MetaError::from(error).into()),
        };
        let cursor = query.cursor.as_ref().map(ArtifactDigest::canonical);
        let entries = cursor
            .as_ref()
            .map_or_else(
                || table.iter(),
                |cursor| table.range::<&str>((Excluded(cursor.as_str()), Unbounded)),
            )
            .map_err(MetaError::from)?;
        let mut records = Vec::with_capacity(query.limit + 1);
        for entry in entries {
            let (_key, value) = entry.map_err(MetaError::from)?;
            let record: DigestRevocation = serde_json::from_slice(value.value()).map_err(MetaError::from)?;
            if query.status.is_some_and(|status| record.state.status() != status) {
                continue;
            }
            records.push(record);
            if records.len() > query.limit {
                break;
            }
        }
        let next_cursor = (records.len() > query.limit).then(|| records[query.limit - 1].digest.canonical());
        records.truncate(query.limit);
        Ok(DigestRevocationPage {
            revocations: records,
            next_cursor,
        })
    }
}
