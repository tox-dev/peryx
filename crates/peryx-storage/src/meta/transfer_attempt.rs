//! Durable identity and progress for the artifact transfers that populate a blob placement.
//!
//! The [`blob_placement`](super::blob_placement) ledger records where a digest ends up and whether it
//! is serveable. This module records the work of getting it there: one current attempt per target
//! placement, its retry sequence, a rate-limited progress checkpoint, and a classified terminal
//! outcome. A worker reads the current attempt after a restart to resume from the last durable offset
//! instead of starting the byte stream over, and a bounded history lets an operator see how a
//! placement was reached.
//!
//! The transfer engine, the remote streaming endpoint, and any UI live above this module. Here only
//! the durable attempt state and its bounded queries exist. A staged temporary file is never counted
//! as verified, and a checkpoint is deliberately allowed to lag the live byte position so a
//! high-frequency stream does not turn into one metadata write per chunk.

use std::collections::BTreeMap;
use std::ops::Bound::{Excluded, Included};

use peryx_identity::ArtifactDigest;
use redb::ReadableTable as _;
use serde::{Deserialize, Serialize};

use super::blob_placement::{BlobPlacementFailure, BlobPlacementKey, DataCenterId};
use super::{MetaError, MetaStore, TRANSFER_ATTEMPT};

/// The most transfer attempts one placement retains before compaction, bounding a placement's attempt
/// history and its per-placement scan.
pub const MAX_ATTEMPTS_PER_PLACEMENT: usize = 32;

const RETENTION_BATCH: usize = 128;

/// Where one transfer attempt stands.
///
/// A staged temporary file is only [`InProgress`](Self::InProgress); the attempt reaches
/// [`Succeeded`](Self::Succeeded) after the delivered bytes hash to the target digest at the exact
/// object size, the sole outcome that proves a placement can serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum TransferAttemptState {
    /// Bytes are moving. `transferred` is the last durably checkpointed offset, which trails the live
    /// position by at most the checkpoint budget.
    InProgress { transferred: u64 },
    /// The attempt ended without a serveable object, for a classified reason.
    Failed { class: BlobPlacementFailure },
    /// The transfer delivered the exact object size and its digest matched the target.
    Succeeded { size: u64 },
}

impl TransferAttemptState {
    #[must_use]
    pub const fn status(&self) -> TransferAttemptStatus {
        match self {
            Self::InProgress { .. } => TransferAttemptStatus::InProgress,
            Self::Failed { .. } => TransferAttemptStatus::Failed,
            Self::Succeeded { .. } => TransferAttemptStatus::Succeeded,
        }
    }

    const fn is_terminal(&self) -> bool {
        matches!(self, Self::Failed { .. } | Self::Succeeded { .. })
    }
}

/// The lifecycle category of an attempt, without its progress or size payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferAttemptStatus {
    InProgress,
    Failed,
    Succeeded,
}

/// A durable transfer attempt toward one blob placement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferAttemptRecord {
    pub key: BlobPlacementKey,
    /// The attempt's position in its placement's retry history, starting at 1; a retry opens the next
    /// sequence rather than mutating the failed one.
    pub sequence: u64,
    pub state: TransferAttemptState,
    /// The object's known total size, the offset a completed transfer must reach.
    pub expected_size: u64,
    /// The data center the bytes are pulled from, retained so a source reselection is visible in the
    /// history; `None` for a copy that stages from a local backend.
    pub source_data_center: Option<DataCenterId>,
    /// The authority epoch that last wrote this attempt; a lower epoch is a stale worker and is fenced
    /// out without changing the record.
    pub fence: u64,
    pub started_at_unix: i64,
    pub updated_at_unix: i64,
    /// When the durable checkpoint last advanced, used to rate-limit progress writes.
    pub checkpointed_at_unix: i64,
}

/// The plan a caller opens a transfer attempt with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferPlan {
    pub expected_size: u64,
    pub source_data_center: Option<DataCenterId>,
}

/// How often a progress update may reach durable storage, so a high-frequency byte stream does not
/// become one metadata write per chunk. The final offset always persists regardless of the budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointPolicy {
    /// The minimum offset advance, in bytes, before a checkpoint persists.
    pub min_bytes: u64,
    /// The minimum seconds between persisted checkpoints.
    pub min_interval_secs: i64,
}

/// The operator-set policy that compaction prunes terminal attempts under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttemptRetention {
    /// Terminal attempts updated within this many seconds are always kept.
    pub max_age_secs: i64,
    /// The count of most-recent terminal attempts to keep per placement regardless of age.
    pub keep_per_placement: usize,
}

/// The effect of opening a transfer attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeginOutcome {
    /// A fresh attempt opened, either the placement's first or a retry after a failure.
    Started(TransferAttemptRecord),
    /// An interrupted in-progress attempt was returned unchanged so the caller resumes it from its
    /// last durable checkpoint.
    Resumed(TransferAttemptRecord),
}

impl BeginOutcome {
    #[must_use]
    pub const fn record(&self) -> &TransferAttemptRecord {
        match self {
            Self::Started(record) | Self::Resumed(record) => record,
        }
    }
}

/// The effect of a progress checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckpointOutcome {
    /// The offset advanced past the write-rate budget and the checkpoint was written.
    Persisted(TransferAttemptRecord),
    /// The offset advanced in memory but stayed within the budget, so nothing was written; the
    /// returned record still shows the last durable offset a restart would resume from.
    Coalesced(TransferAttemptRecord),
}

impl CheckpointOutcome {
    #[must_use]
    pub const fn record(&self) -> &TransferAttemptRecord {
        match self {
            Self::Persisted(record) | Self::Coalesced(record) => record,
        }
    }
}

/// A bounded per-label count of transfer attempts, the low-cardinality shape a metrics exporter reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TransferAttemptMetric {
    pub data_center: String,
    pub backend: String,
    pub state: TransferAttemptStatus,
    /// The failure class, present only for a failed state, so an exporter labels retryable and
    /// terminal failures apart.
    pub error_class: Option<BlobPlacementFailure>,
    pub count: u64,
}

/// A rejected transfer-attempt operation.
#[derive(Debug, thiserror::Error)]
pub enum TransferAttemptError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("a newer fence {current} supersedes the applied fence {applied}")]
    StaleFence { current: u64, applied: u64 },
    #[error("no open transfer attempt exists for this placement")]
    NoOpenAttempt,
    #[error("the current transfer attempt already succeeded")]
    AlreadySucceeded,
    #[error("a placement cannot exceed {MAX_ATTEMPTS_PER_PLACEMENT} transfer attempts")]
    TooManyAttempts,
    #[error("a checkpoint offset {offset} exceeds the expected size {expected_size}")]
    OffsetPastEnd { offset: u64, expected_size: u64 },
}

fn attempt_key(placement: &str, sequence: u64) -> String {
    format!("{placement}\0{sequence:016x}")
}

fn placement_bounds(placement: &str) -> (String, String) {
    (format!("{placement}\0"), format!("{placement}\u{1}"))
}

impl MetaStore {
    /// Open or resume the transfer attempt for one placement under a fencing epoch.
    ///
    /// An in-progress attempt is returned as [`Resumed`](BeginOutcome::Resumed) so a worker that
    /// restarted picks up its last durable checkpoint. A resume that carries a newer fence than the
    /// persisted one writes that fence in the same transaction, so the resuming worker claims authority
    /// at once and a superseded worker's later checkpoint is fenced out instead of landing in the window
    /// before the resuming worker's first write; a resume at the same fence leaves the record untouched.
    /// A first call, or one after a failed attempt, opens the next [`Started`](BeginOutcome::Started)
    /// sequence.
    ///
    /// # Errors
    /// Returns [`TransferAttemptError`] for a stale fence, a current attempt that already succeeded, or
    /// a placement already at [`MAX_ATTEMPTS_PER_PLACEMENT`], or a store error when a row cannot be
    /// read, encoded, or committed.
    pub fn begin_transfer_attempt(
        &self,
        key: &BlobPlacementKey,
        plan: &TransferPlan,
        fence: u64,
        now: i64,
    ) -> Result<BeginOutcome, TransferAttemptError> {
        let placement = key.encode();
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        let current = read_current(&txn, &placement)?;
        if let Some((_, record)) = &current
            && fence < record.fence
        {
            return Err(TransferAttemptError::StaleFence {
                current: record.fence,
                applied: fence,
            });
        }
        let sequence = match &current {
            Some((sequence, record)) => match record.state {
                TransferAttemptState::InProgress { .. } => {
                    if fence <= record.fence {
                        return Ok(BeginOutcome::Resumed(record.clone()));
                    }
                    let mut resumed = record.clone();
                    resumed.fence = fence;
                    resumed.updated_at_unix = now;
                    write_at(&txn, &placement, *sequence, &resumed)?;
                    txn.commit().map_err(MetaError::from)?;
                    return Ok(BeginOutcome::Resumed(resumed));
                }
                TransferAttemptState::Succeeded { .. } => return Err(TransferAttemptError::AlreadySucceeded),
                TransferAttemptState::Failed { .. } => {
                    guard_capacity(&txn, &placement)?;
                    record.sequence + 1
                }
            },
            None => 1,
        };
        let record = TransferAttemptRecord {
            key: key.clone(),
            sequence,
            state: TransferAttemptState::InProgress { transferred: 0 },
            expected_size: plan.expected_size,
            source_data_center: plan.source_data_center.clone(),
            fence,
            started_at_unix: now,
            updated_at_unix: now,
            checkpointed_at_unix: now,
        };
        write_record(&txn, &placement, &record)?;
        txn.commit().map_err(MetaError::from)?;
        Ok(BeginOutcome::Started(record))
    }

    /// Advance the durable progress checkpoint of the current attempt, subject to the write-rate
    /// budget.
    ///
    /// A `transferred` offset that reaches `expected_size` always persists so a completed transfer's
    /// progress is exact; otherwise the offset persists only once it has advanced by `min_bytes` or
    /// `min_interval_secs` since the last write, and is [`Coalesced`](CheckpointOutcome::Coalesced)
    /// without a write in between. An offset at or below the last durable one never regresses the
    /// checkpoint.
    ///
    /// # Errors
    /// Returns [`TransferAttemptError`] when no attempt is in progress, the offset exceeds
    /// `expected_size`, or the fence is stale, or a store error when a row cannot be read, encoded, or
    /// committed.
    pub fn checkpoint_transfer_attempt(
        &self,
        key: &BlobPlacementKey,
        transferred: u64,
        policy: CheckpointPolicy,
        fence: u64,
        now: i64,
    ) -> Result<CheckpointOutcome, TransferAttemptError> {
        let placement = key.encode();
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        let Some((sequence, mut record)) = read_current(&txn, &placement)? else {
            return Err(TransferAttemptError::NoOpenAttempt);
        };
        if fence < record.fence {
            return Err(TransferAttemptError::StaleFence {
                current: record.fence,
                applied: fence,
            });
        }
        let TransferAttemptState::InProgress { transferred: durable } = record.state else {
            return Err(TransferAttemptError::NoOpenAttempt);
        };
        if transferred > record.expected_size {
            return Err(TransferAttemptError::OffsetPastEnd {
                offset: transferred,
                expected_size: record.expected_size,
            });
        }
        let reached_end = transferred == record.expected_size;
        let advanced = transferred > durable
            && (reached_end
                || transferred - durable >= policy.min_bytes
                || now - record.checkpointed_at_unix >= policy.min_interval_secs);
        if !advanced {
            return Ok(CheckpointOutcome::Coalesced(record));
        }
        record.state = TransferAttemptState::InProgress { transferred };
        record.fence = record.fence.max(fence);
        record.updated_at_unix = now;
        record.checkpointed_at_unix = now;
        write_at(&txn, &placement, sequence, &record)?;
        txn.commit().map_err(MetaError::from)?;
        Ok(CheckpointOutcome::Persisted(record))
    }

    /// Mark the current in-progress attempt failed with a classified reason.
    ///
    /// A source that could not be reached records [`SourceUnavailable`](BlobPlacementFailure), which a
    /// caller answers with a retry or a reselected source; the failed attempt is retained as history
    /// and never erases a verified placement in the [`blob_placement`](super::blob_placement) ledger. A
    /// call against an already-failed attempt is a no-op.
    ///
    /// # Errors
    /// Returns [`TransferAttemptError`] when no attempt exists, the current attempt already succeeded,
    /// or the fence is stale, or a store error when a row cannot be read, encoded, or committed.
    pub fn fail_transfer_attempt(
        &self,
        key: &BlobPlacementKey,
        class: BlobPlacementFailure,
        fence: u64,
        now: i64,
    ) -> Result<TransferAttemptRecord, TransferAttemptError> {
        self.finish_attempt(key, fence, now, |_| TransferAttemptState::Failed { class })
    }

    /// Complete the current in-progress attempt with the backend's observed digest and byte size.
    ///
    /// An observed digest that does not match the placement's target records a
    /// [`DigestMismatch`](BlobPlacementFailure::DigestMismatch) failure, which can never serve, rather
    /// than a success. A match records [`Succeeded`](TransferAttemptState::Succeeded) at `size`.
    ///
    /// # Errors
    /// Returns [`TransferAttemptError`] when no attempt is in progress, the current attempt already
    /// succeeded, or the fence is stale, or a store error when a row cannot be read, encoded, or
    /// committed.
    pub fn complete_transfer_attempt(
        &self,
        key: &BlobPlacementKey,
        observed: &ArtifactDigest,
        size: u64,
        fence: u64,
        now: i64,
    ) -> Result<TransferAttemptRecord, TransferAttemptError> {
        self.finish_attempt(key, fence, now, |record| {
            if observed == &key.digest && size == record.expected_size {
                TransferAttemptState::Succeeded { size }
            } else {
                TransferAttemptState::Failed {
                    class: BlobPlacementFailure::DigestMismatch,
                }
            }
        })
    }

    /// Read the current transfer attempt for one placement.
    ///
    /// # Errors
    /// Returns a store error when the row cannot be read or decoded.
    pub fn transfer_attempt(&self, key: &BlobPlacementKey) -> Result<Option<TransferAttemptRecord>, MetaError> {
        let placement = key.encode();
        let txn = self.db.begin_read()?;
        Ok(read_current_read(&txn, &placement)?.map(|(_, record)| record))
    }

    /// List every transfer attempt for one digest across its placements, in key then sequence order.
    ///
    /// # Errors
    /// Returns a store error when a row cannot be read or decoded.
    pub fn transfer_attempts(&self, digest: &ArtifactDigest) -> Result<Vec<TransferAttemptRecord>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(TRANSFER_ATTEMPT)?;
        let (low, high) = BlobPlacementKey::digest_bounds(digest);
        let mut records = Vec::new();
        for entry in table.range::<&str>((Included(low.as_str()), Excluded(high.as_str())))? {
            let (_key, value) = entry?;
            records.push(serde_json::from_slice(value.value())?);
        }
        Ok(records)
    }

    /// Remove one bounded batch of terminal attempts a retention policy no longer requires, keeping
    /// every in-progress attempt and the newest terminal ones per placement.
    ///
    /// Returns the number of attempts removed; a caller loops until it returns fewer than the batch
    /// size to drain a large backlog without holding one long transaction.
    ///
    /// # Errors
    /// Returns a store error when history cannot be read, decoded, or committed.
    pub fn compact_transfer_attempts(&self, retention: AttemptRetention, now: i64) -> Result<usize, MetaError> {
        let txn = self.db.begin_write()?;
        let stale = collect_prunable(&txn, retention, now)?;
        let removed = stale.len();
        {
            let mut table = txn.open_table(TRANSFER_ATTEMPT)?;
            for key in stale {
                table.remove(key.as_str())?;
            }
        }
        txn.commit()?;
        Ok(removed)
    }

    /// Aggregate transfer attempts into bounded per-label counts for a metrics exporter.
    ///
    /// Series are keyed by data center, backend, state, and, for failures, error class. Digest,
    /// location, sequence, and operation identity are excluded, so cardinality stays within the
    /// topology's backends and data centers times the fixed state and failure classes.
    ///
    /// # Errors
    /// Returns a store error when a row cannot be read or decoded.
    pub fn transfer_attempt_metrics(&self) -> Result<Vec<TransferAttemptMetric>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(TRANSFER_ATTEMPT)?;
        let mut counts: BTreeMap<(String, String, TransferAttemptStatus, Option<BlobPlacementFailure>), u64> =
            BTreeMap::new();
        for entry in table.iter()? {
            let (_key, value) = entry?;
            let record: TransferAttemptRecord = serde_json::from_slice(value.value())?;
            let error_class = match record.state {
                TransferAttemptState::Failed { class } => Some(class),
                _ => None,
            };
            let label = (
                record.key.data_center.as_str().to_owned(),
                record.key.backend.as_str().to_owned(),
                record.state.status(),
                error_class,
            );
            *counts.entry(label).or_default() += 1;
        }
        Ok(counts
            .into_iter()
            .map(
                |((data_center, backend, state, error_class), count)| TransferAttemptMetric {
                    data_center,
                    backend,
                    state,
                    error_class,
                    count,
                },
            )
            .collect())
    }

    fn finish_attempt(
        &self,
        key: &BlobPlacementKey,
        fence: u64,
        now: i64,
        next: impl FnOnce(&TransferAttemptRecord) -> TransferAttemptState,
    ) -> Result<TransferAttemptRecord, TransferAttemptError> {
        let placement = key.encode();
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        let Some((sequence, mut record)) = read_current(&txn, &placement)? else {
            return Err(TransferAttemptError::NoOpenAttempt);
        };
        if fence < record.fence {
            return Err(TransferAttemptError::StaleFence {
                current: record.fence,
                applied: fence,
            });
        }
        match record.state {
            TransferAttemptState::Succeeded { .. } => return Err(TransferAttemptError::AlreadySucceeded),
            TransferAttemptState::Failed { .. } => return Ok(record),
            TransferAttemptState::InProgress { .. } => {}
        }
        record.state = next(&record);
        record.fence = record.fence.max(fence);
        record.updated_at_unix = now;
        write_at(&txn, &placement, sequence, &record)?;
        txn.commit().map_err(MetaError::from)?;
        Ok(record)
    }
}

fn read_current(
    txn: &redb::WriteTransaction,
    placement: &str,
) -> Result<Option<(u64, TransferAttemptRecord)>, MetaError> {
    let table = txn.open_table(TRANSFER_ATTEMPT)?;
    let (low, high) = placement_bounds(placement);
    let Some(entry) = table
        .range::<&str>((Included(low.as_str()), Excluded(high.as_str())))?
        .next_back()
    else {
        return Ok(None);
    };
    let (_key, value) = entry?;
    let record: TransferAttemptRecord = serde_json::from_slice(value.value())?;
    Ok(Some((record.sequence, record)))
}

fn read_current_read(
    txn: &redb::ReadTransaction,
    placement: &str,
) -> Result<Option<(u64, TransferAttemptRecord)>, MetaError> {
    let table = txn.open_table(TRANSFER_ATTEMPT)?;
    let (low, high) = placement_bounds(placement);
    let Some(entry) = table
        .range::<&str>((Included(low.as_str()), Excluded(high.as_str())))?
        .next_back()
    else {
        return Ok(None);
    };
    let (_key, value) = entry?;
    let record: TransferAttemptRecord = serde_json::from_slice(value.value())?;
    Ok(Some((record.sequence, record)))
}

fn write_record(
    txn: &redb::WriteTransaction,
    placement: &str,
    record: &TransferAttemptRecord,
) -> Result<(), MetaError> {
    write_at(txn, placement, record.sequence, record)
}

fn write_at(
    txn: &redb::WriteTransaction,
    placement: &str,
    sequence: u64,
    record: &TransferAttemptRecord,
) -> Result<(), MetaError> {
    let value = serde_json::to_vec(record)?;
    txn.open_table(TRANSFER_ATTEMPT)?
        .insert(attempt_key(placement, sequence).as_str(), value.as_slice())?;
    Ok(())
}

fn guard_capacity(txn: &redb::WriteTransaction, placement: &str) -> Result<(), TransferAttemptError> {
    let (low, high) = placement_bounds(placement);
    let table = txn.open_table(TRANSFER_ATTEMPT).map_err(MetaError::from)?;
    let count = table
        .range::<&str>((Included(low.as_str()), Excluded(high.as_str())))
        .map_err(MetaError::from)?
        .count();
    if count >= MAX_ATTEMPTS_PER_PLACEMENT {
        return Err(TransferAttemptError::TooManyAttempts);
    }
    Ok(())
}

fn collect_prunable(
    txn: &redb::WriteTransaction,
    retention: AttemptRetention,
    now: i64,
) -> Result<Vec<String>, MetaError> {
    let table = txn.open_table(TRANSFER_ATTEMPT)?;
    let mut stale = Vec::new();
    let mut group: Vec<(String, TransferAttemptRecord)> = Vec::new();
    let mut group_placement: Option<String> = None;
    for entry in table.iter()? {
        let (raw_key, value) = entry?;
        let record: TransferAttemptRecord = serde_json::from_slice(value.value())?;
        let placement = record.key.encode();
        if group_placement.as_ref() != Some(&placement) {
            flush_group(&mut group, retention, now, &mut stale);
            group_placement = Some(placement);
        }
        group.push((raw_key.value().to_owned(), record));
    }
    flush_group(&mut group, retention, now, &mut stale);
    stale.truncate(RETENTION_BATCH);
    Ok(stale)
}

fn flush_group(
    group: &mut Vec<(String, TransferAttemptRecord)>,
    retention: AttemptRetention,
    now: i64,
    stale: &mut Vec<String>,
) {
    let terminal = group.iter().filter(|(_, record)| record.state.is_terminal()).count();
    let mut prunable = terminal.saturating_sub(retention.keep_per_placement);
    for (key, record) in group.drain(..) {
        if prunable == 0 {
            break;
        }
        if record.state.is_terminal() && now.saturating_sub(record.updated_at_unix) > retention.max_age_secs {
            stale.push(key);
            prunable -= 1;
        }
    }
}
