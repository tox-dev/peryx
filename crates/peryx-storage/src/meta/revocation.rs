use std::ops::Bound::{Excluded, Included, Unbounded};

use peryx_identity::{ArtifactDigest, RevocationReason, UserId};
use redb::{ReadableTable as _, ReadableTableMetadata as _};
use serde::{Deserialize, Serialize};

use super::{
    DIGEST_REVOCATION, DIGEST_REVOCATION_BY_STATUS, DIGEST_REVOCATION_STATE, JournalEntry, MetaError, MetaStore,
    ServerMutation, open_optional_table,
};

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

impl DigestRevocationStatus {
    /// The separator sorts below every canonical digest character, so each status owns a contiguous
    /// key range that a prefix test can close.
    const fn index_prefix(self) -> &'static str {
        match self {
            Self::Active => "active\0",
            Self::Lifted => "lifted\0",
        }
    }
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

/// Rebuilds the derived count and the status index for legacy or drifted stores.
///
/// An undecodable row aborts before any derived table is written, so a corrupt store never gains an
/// index that claims to describe it.
///
/// # Errors
/// Returns a store error when the tables cannot be opened, read, or written.
pub(super) fn backfill_digest_revocation_state(txn: &redb::WriteTransaction) -> Result<(), MetaError> {
    let records = txn.open_table(DIGEST_REVOCATION)?;
    let mut index = txn.open_table(DIGEST_REVOCATION_BY_STATUS)?;
    let mut active: u64 = 0;
    let mut rows: u64 = 0;
    let mut indexed = true;
    for entry in records.iter()? {
        let (key, value) = entry?;
        let record = serde_json::from_slice::<DigestRevocation>(value.value())?;
        if record.state == DigestRevocationState::Active {
            active += 1;
        }
        rows += 1;
        if index
            .get(index_key(record.state.status(), key.value()).as_str())?
            .is_none()
        {
            indexed = false;
        }
    }
    if !indexed || index.len()? != rows {
        while index.pop_first()?.is_some() {}
        for entry in records.iter()? {
            let (key, value) = entry?;
            let record = serde_json::from_slice::<DigestRevocation>(value.value())?;
            index.insert(index_key(record.state.status(), key.value()).as_str(), ())?;
        }
    }
    let mut state = txn.open_table(DIGEST_REVOCATION_STATE)?;
    if state.get(ACTIVE_COUNT_KEY)?.map_or(0, |count| count.value()) != active {
        state.insert(ACTIVE_COUNT_KEY, active)?;
    }
    Ok(())
}

fn index_key(status: DigestRevocationStatus, digest: &str) -> String {
    format!("{}{digest}", status.index_prefix())
}

/// Replaces every revocation with `records`, rebuilding the status index and the active count.
///
/// A checkpoint install is a replacement, so the count is recomputed from what arrives rather than
/// stepped from what was there: stepping would underflow on a lifted record whose active predecessor
/// the install had just removed.
///
/// # Errors
/// Returns a store error when a table cannot be opened, written, or encoded.
pub(super) fn replace_digest_revocations(
    txn: &redb::WriteTransaction,
    records: &std::collections::BTreeMap<String, DigestRevocation>,
) -> Result<(), MetaError> {
    txn.delete_table(DIGEST_REVOCATION)?;
    txn.delete_table(DIGEST_REVOCATION_BY_STATUS)?;
    txn.delete_table(DIGEST_REVOCATION_STATE)?;
    let mut rows = txn.open_table(DIGEST_REVOCATION)?;
    let mut index = txn.open_table(DIGEST_REVOCATION_BY_STATUS)?;
    let mut active = 0_u64;
    for (digest, record) in records {
        let status = record.state.status();
        rows.insert(digest.as_str(), serde_json::to_vec(record)?.as_slice())?;
        index.insert(index_key(status, digest).as_str(), ())?;
        if status == DigestRevocationStatus::Active {
            active += 1;
        }
    }
    drop(rows);
    drop(index);
    txn.open_table(DIGEST_REVOCATION_STATE)?
        .insert(ACTIVE_COUNT_KEY, active)?;
    Ok(())
}

/// Writes the row, the status index, and the active count together in the caller's transaction.
///
/// Every writer goes through here: an operator action on the primary, and journal replay on a replica.
/// A replica that maintained the index at its own write site could drift from the primary silently,
/// because a status-filtered page reads the index alone and would simply omit the rows it lacks.
///
/// # Errors
/// Returns a store error when the row cannot be read, decoded, encoded, or written, or when the active
/// count would leave `u64`.
pub(super) fn apply_digest_revocation(
    txn: &redb::WriteTransaction,
    record: &DigestRevocation,
) -> Result<(), MetaError> {
    let key = record.digest.canonical();
    let status = record.state.status();
    let mut records = txn.open_table(DIGEST_REVOCATION)?;
    let previous = records
        .get(key.as_str())?
        .map(|value| serde_json::from_slice::<DigestRevocation>(value.value()))
        .transpose()?
        .map(|stored| stored.state.status());
    records.insert(key.as_str(), serde_json::to_vec(record)?.as_slice())?;
    drop(records);
    if previous == Some(status) {
        return Ok(());
    }
    let mut index = txn.open_table(DIGEST_REVOCATION_BY_STATUS)?;
    if let Some(previous) = previous {
        index.remove(index_key(previous, &key).as_str())?;
    }
    index.insert(index_key(status, &key).as_str(), ())?;
    drop(index);
    let mut state = txn.open_table(DIGEST_REVOCATION_STATE)?;
    let active = state.get(ACTIVE_COUNT_KEY)?.map_or(0, |count| count.value());
    let active = match status {
        DigestRevocationStatus::Active => active
            .checked_add(1)
            .ok_or_else(|| MetaError::DriverPrecondition("digest revocation active count overflow".to_owned()))?,
        DigestRevocationStatus::Lifted => active
            .checked_sub(1)
            .ok_or_else(|| MetaError::DriverPrecondition("digest revocation active count underflow".to_owned()))?,
    };
    state.insert(ACTIVE_COUNT_KEY, active)?;
    Ok(())
}

/// Appends the change to the journal so replicas replay it in serial order with everything else.
///
/// A revocation is a security response, so it is the writer's state that has to reach the followers:
/// the entry carries the whole row rather than a hint to re-read, which no follower could act on
/// without querying the primary it is trying to stay independent of.
fn journal_digest_revocation(txn: &redb::WriteTransaction, record: &DigestRevocation) -> Result<(), MetaError> {
    super::index::commit_journal::<MetaError>(
        txn,
        &[JournalEntry {
            payload: ServerMutation::DigestRevocation { record: record.clone() }.encode(),
            mutations: Vec::new(),
            blobs: Vec::new(),
        }],
    )
    .map(|_| ())
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
        apply_digest_revocation(&txn, &record)?;
        journal_digest_revocation(&txn, &record)?;
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
        apply_digest_revocation(&txn, &record)?;
        journal_digest_revocation(&txn, &record)?;
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
    /// A status filter walks the status index instead of the primary table, so the page costs work
    /// proportional to its limit rather than to the number of rows in the other status.
    ///
    /// # Errors
    /// Returns a validation error for an out-of-range limit, or a store error when the status index is
    /// absent or inconsistent, or when rows cannot be read or decoded.
    pub fn query_digest_revocations(
        &self,
        query: &DigestRevocationQuery,
    ) -> Result<DigestRevocationPage, DigestRevocationQueryError> {
        if !(1..=MAX_QUERY_LIMIT).contains(&query.limit) {
            return Err(DigestRevocationQueryError::InvalidLimit);
        }
        let txn = self.db.begin_read().map_err(MetaError::from)?;
        let Some(table) = open_optional_table(&txn, DIGEST_REVOCATION)? else {
            return Ok(DigestRevocationPage {
                revocations: Vec::new(),
                next_cursor: None,
            });
        };
        let cursor = query.cursor.as_ref().map(ArtifactDigest::canonical);
        let mut records = match query.status {
            Some(status) => status_page(&txn, &table, status, cursor.as_deref(), query.limit)?,
            None => digest_page(&table, cursor.as_deref(), query.limit)?,
        };
        let next_cursor = (records.len() > query.limit).then(|| records[query.limit - 1].digest.canonical());
        records.truncate(query.limit);
        Ok(DigestRevocationPage {
            revocations: records,
            next_cursor,
        })
    }
}

/// Reads at most `limit + 1` index entries, each resolved by a point lookup, so rows carrying the
/// other status are never visited.
fn status_page(
    txn: &redb::ReadTransaction,
    table: &redb::ReadOnlyTable<&'static str, &'static [u8]>,
    status: DigestRevocationStatus,
    cursor: Option<&str>,
    limit: usize,
) -> Result<Vec<DigestRevocation>, DigestRevocationQueryError> {
    let Some(index) = open_optional_table(txn, DIGEST_REVOCATION_BY_STATUS)? else {
        return Err(MetaError::DriverPrecondition("digest revocation index is incomplete".to_owned()).into());
    };
    let prefix = status.index_prefix();
    let start = cursor.map(|cursor| index_key(status, cursor));
    let lower = start
        .as_ref()
        .map_or(Included(prefix), |start| Excluded(start.as_str()));
    let mut records = Vec::with_capacity(limit + 1);
    for entry in index.range::<&str>((lower, Unbounded)).map_err(MetaError::from)? {
        let (key, _) = entry.map_err(MetaError::from)?;
        let Some(digest) = key.value().strip_prefix(prefix) else {
            break;
        };
        let Some(value) = table.get(digest).map_err(MetaError::from)? else {
            return Err(
                MetaError::DriverPrecondition("digest revocation index references a missing row".to_owned()).into(),
            );
        };
        records.push(serde_json::from_slice(value.value()).map_err(MetaError::from)?);
        if records.len() > limit {
            break;
        }
    }
    Ok(records)
}

fn digest_page(
    table: &redb::ReadOnlyTable<&'static str, &'static [u8]>,
    cursor: Option<&str>,
    limit: usize,
) -> Result<Vec<DigestRevocation>, DigestRevocationQueryError> {
    let entries = cursor
        .map_or_else(
            || table.iter(),
            |cursor| table.range::<&str>((Excluded(cursor), Unbounded)),
        )
        .map_err(MetaError::from)?;
    let mut records = Vec::with_capacity(limit + 1);
    for entry in entries {
        let (_key, value) = entry.map_err(MetaError::from)?;
        records.push(serde_json::from_slice(value.value()).map_err(MetaError::from)?);
        if records.len() > limit {
            break;
        }
    }
    Ok(records)
}
