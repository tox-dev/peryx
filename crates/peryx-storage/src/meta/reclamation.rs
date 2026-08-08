//! The storage-side safety selector for replicated blob reclamation.
//!
//! The single-node orphan collector in `peryx`'s cache purge decides a blob is unreferenced from a
//! reference snapshot and deletes it. In a replicated topology that snapshot is not enough: a lagging
//! replica or backup can still be replaying the very metadata that names the blob, so deleting its
//! bytes now would strand a plane that has not yet caught up. This module records a durable
//! *reclamation tombstone* per digest that gates deletion behind three proofs - no live reference, no
//! placement a replica can still serve, and every plane advanced past the selection frontier - all
//! under a fencing epoch so a superseded worker cannot regress the decision.
//!
//! It is a pure decision core. The fence epoch and the observed replica and backup frontiers are
//! supplied by the caller; this module never sources them, because the cluster authority that mints
//! the fence and the replication loop that reports the frontiers live above it and are still being
//! built. The reference verdict likewise crosses the driver boundary and is passed in. What lives here
//! is only the durable tombstone, its fenced transitions, and the bounded queries an operator reads.
//! Executors delete the bytes; the orchestrator drives the process to completion.

use std::ops::Bound::{Excluded, Included};

use peryx_identity::ArtifactDigest;
use redb::ReadableTable as _;
use serde::{Deserialize, Serialize};

use super::blob_placement::{BlobPlacementKey, BlobPlacementRecord, BlobPlacementState};
use super::{BLOB_PLACEMENT, MetaError, MetaStore, RECLAMATION_TOMBSTONE};

/// Where a reclamation tombstone stands in its safety pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum ReclamationState {
    /// Selected as unreferenced and unservable, waiting for every replica and backup to advance past
    /// the recorded [`required_frontier`](ReclamationTombstone::required_frontier) before its bytes may
    /// be deleted.
    Pending,
    /// Every plane cleared the required frontier and a final reference and serveability re-check passed
    /// under the current fence, so a backend-delete executor may now remove the bytes.
    Ready,
    /// The candidate was abandoned rather than deleted, for a classified reason, because a proof it
    /// depends on no longer holds.
    Skipped { reason: SkipReason },
}

impl ReclamationState {
    #[must_use]
    pub const fn status(&self) -> ReclamationStatus {
        match self {
            Self::Pending => ReclamationStatus::Pending,
            Self::Ready => ReclamationStatus::Ready,
            Self::Skipped { .. } => ReclamationStatus::Skipped,
        }
    }
}

/// The lifecycle category of a tombstone, without its skip payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReclamationStatus {
    Pending,
    Ready,
    Skipped,
}

/// Why a candidate was abandoned instead of deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// Metadata named the digest again after it was selected.
    Referenced,
    /// A verified placement can serve the digest again, so a replica still needs its bytes.
    Serveable,
}

/// A durable decision to reclaim one content-addressed digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReclamationTombstone {
    pub digest: ArtifactDigest,
    pub state: ReclamationState,
    /// The authoritative metadata serial in force when the digest was selected. Every replica and
    /// backup must have applied through it before the bytes may be deleted, so a plane still
    /// reconstructing state from not-yet-applied metadata that names the digest keeps its bytes.
    pub required_frontier: u64,
    /// The authority epoch that owns this reclamation; a lower epoch is a superseded worker and is
    /// fenced out without changing the tombstone.
    pub fence: u64,
    /// How many fenced advances this tombstone has taken, so an operator sees retry pressure and a
    /// reader can detect a concurrent change.
    pub attempts: u64,
    pub selected_at_unix: i64,
    pub updated_at_unix: i64,
}

/// The frontier each configured replication plane has observably applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservedFrontier {
    /// The lowest serial any live replica has applied.
    pub replica: Option<u64>,
    /// The lowest serial any configured backup has captured.
    pub backup: Option<u64>,
}

impl ObservedFrontier {
    /// An absent plane imposes no frontier requirement.
    #[must_use]
    pub const fn covers(&self, required: u64) -> bool {
        let replica = match self.replica {
            Some(frontier) => frontier >= required,
            None => true,
        };
        let backup = match self.backup {
            Some(frontier) => frontier >= required,
            None => true,
        };
        replica && backup
    }
}

/// The effect of selecting a reclamation candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectOutcome {
    /// A tombstone was armed at [`Pending`](ReclamationState::Pending), awaiting its frontiers.
    Selected(ReclamationTombstone),
    /// An existing tombstone was abandoned because a proof no longer held at selection time.
    Skipped(ReclamationTombstone),
    /// The digest was ineligible and no tombstone existed, so nothing was persisted.
    Ineligible(SkipReason),
}

/// The effect of a readiness check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadyOutcome {
    /// Every gate passed; the tombstone is [`Ready`](ReclamationState::Ready) for byte deletion.
    Ready(ReclamationTombstone),
    /// A plane has not yet reached the required frontier, so the tombstone stays
    /// [`Pending`](ReclamationState::Pending) for a later retry.
    NotReady {
        tombstone: ReclamationTombstone,
        observed: ObservedFrontier,
    },
    /// A reference or a serveable placement reappeared, so the candidate was abandoned.
    Skipped(ReclamationTombstone),
}

/// A rejected reclamation operation.
#[derive(Debug, thiserror::Error)]
pub enum ReclamationError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("a newer fence {current} supersedes the applied fence {applied}")]
    StaleFence { current: u64, applied: u64 },
    #[error("no reclamation candidate exists for this digest")]
    MissingCandidate,
}

/// The bounded backlog of reclamation tombstones by state, the low-cardinality progress a metrics
/// exporter or an operator's vacuum-style view reads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ReclamationProgress {
    pub pending: u64,
    pub ready: u64,
    pub skipped: u64,
}

impl MetaStore {
    /// Select `digest` for reclamation under a fencing epoch, or abandon it if a proof already fails.
    ///
    /// `referenced` is the caller's fresh verdict on whether any ecosystem's metadata still names the
    /// digest; serveability is read in the same transaction and mirrors
    /// [`route_blob_placements`](Self::route_blob_placements)'s `is_serveable`, so a placement a replica
    /// can serve blocks selection atomically. An eligible digest is armed at
    /// [`Pending`](ReclamationState::Pending) with `required_frontier` - a re-selection raises the
    /// frontier bar and re-arms even a candidate that had reached [`Ready`](ReclamationState::Ready), so
    /// a reference or frontier that changed after it was marked ready is re-proven before deletion. A
    /// digest that is referenced or serveable abandons an existing tombstone to
    /// [`Skipped`](ReclamationState::Skipped) and, when none exists, persists nothing.
    ///
    /// # Errors
    /// Returns [`ReclamationError::StaleFence`] when `fence` is below the tombstone's epoch, or a store
    /// error when a row cannot be read, encoded, or committed.
    pub fn select_reclamation_candidate(
        &self,
        digest: &ArtifactDigest,
        referenced: bool,
        required_frontier: u64,
        fence: u64,
        now: i64,
    ) -> Result<SelectOutcome, ReclamationError> {
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        let existing = read_tombstone(&txn, digest)?;
        guard_fence(existing.as_ref(), fence)?;
        let skip = if referenced {
            Some(SkipReason::Referenced)
        } else if digest_is_serveable(&txn, digest)? {
            Some(SkipReason::Serveable)
        } else {
            None
        };
        let outcome = match (skip, existing) {
            (Some(reason), Some(prior)) => {
                let record = advance(
                    &prior,
                    ReclamationState::Skipped { reason },
                    prior.required_frontier,
                    fence,
                    now,
                );
                write_tombstone(&txn, &record)?;
                SelectOutcome::Skipped(record)
            }
            (Some(reason), None) => SelectOutcome::Ineligible(reason),
            (None, prior) => {
                let raised = prior.as_ref().map_or(required_frontier, |record| {
                    record.required_frontier.max(required_frontier)
                });
                let record = prior.as_ref().map_or_else(
                    || ReclamationTombstone {
                        digest: digest.clone(),
                        state: ReclamationState::Pending,
                        required_frontier: raised,
                        fence,
                        attempts: 1,
                        selected_at_unix: now,
                        updated_at_unix: now,
                    },
                    |record| advance(record, ReclamationState::Pending, raised, fence, now),
                );
                write_tombstone(&txn, &record)?;
                SelectOutcome::Selected(record)
            }
        };
        txn.commit().map_err(MetaError::from)?;
        Ok(outcome)
    }

    /// Run the final safety check for `digest` under the current fence and mark it ready when every gate
    /// passes.
    ///
    /// The reference verdict and observed frontiers are re-taken by the caller; serveability is re-read
    /// in this transaction. A reference or serveable placement that reappeared abandons the candidate to
    /// [`Skipped`](ReclamationState::Skipped). A plane short of the required frontier leaves the
    /// tombstone [`Pending`](ReclamationState::Pending) for a later retry. Only a candidate that is
    /// unreferenced, unservable, and covered by both frontiers reaches [`Ready`](ReclamationState::Ready)
    /// . A tombstone already in a terminal state is returned unchanged.
    ///
    /// # Errors
    /// Returns [`ReclamationError::MissingCandidate`] when no tombstone exists,
    /// [`ReclamationError::StaleFence`] when `fence` is below its epoch, or a store error when a row
    /// cannot be read, encoded, or committed.
    pub fn mark_reclamation_ready(
        &self,
        digest: &ArtifactDigest,
        referenced: bool,
        observed: ObservedFrontier,
        fence: u64,
        now: i64,
    ) -> Result<ReadyOutcome, ReclamationError> {
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        let Some(tombstone) = read_tombstone(&txn, digest)? else {
            return Err(ReclamationError::MissingCandidate);
        };
        guard_fence(Some(&tombstone), fence)?;
        let outcome = match tombstone.state {
            ReclamationState::Ready => ReadyOutcome::Ready(tombstone),
            ReclamationState::Skipped { .. } => ReadyOutcome::Skipped(tombstone),
            ReclamationState::Pending => {
                let required = tombstone.required_frontier;
                if let Some(reason) = terminal_skip(&txn, digest, referenced)? {
                    let record = advance(&tombstone, ReclamationState::Skipped { reason }, required, fence, now);
                    write_tombstone(&txn, &record)?;
                    ReadyOutcome::Skipped(record)
                } else if observed.covers(required) {
                    let record = advance(&tombstone, ReclamationState::Ready, required, fence, now);
                    write_tombstone(&txn, &record)?;
                    ReadyOutcome::Ready(record)
                } else {
                    let record = advance(&tombstone, ReclamationState::Pending, required, fence, now);
                    write_tombstone(&txn, &record)?;
                    ReadyOutcome::NotReady {
                        tombstone: record,
                        observed,
                    }
                }
            }
        };
        txn.commit().map_err(MetaError::from)?;
        Ok(outcome)
    }

    /// Drop the reclamation tombstone for `digest` under a fencing epoch, returning whether one was
    /// removed.
    ///
    /// A delete executor calls this once the bytes are gone; an operator uses it to retract a decision.
    /// A stale fence is rejected so a superseded worker cannot erase a live tombstone.
    ///
    /// # Errors
    /// Returns [`ReclamationError::StaleFence`] when `fence` is below the tombstone's epoch, or a store
    /// error when the row cannot be read or committed.
    pub fn forget_reclamation_tombstone(&self, digest: &ArtifactDigest, fence: u64) -> Result<bool, ReclamationError> {
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        let existing = read_tombstone(&txn, digest)?;
        guard_fence(existing.as_ref(), fence)?;
        let removed = existing.is_some();
        if removed {
            txn.open_table(RECLAMATION_TOMBSTONE)
                .map_err(MetaError::from)?
                .remove(digest.canonical().as_str())
                .map_err(MetaError::from)?;
        }
        txn.commit().map_err(MetaError::from)?;
        Ok(removed)
    }

    /// Read the reclamation tombstone for one digest.
    ///
    /// # Errors
    /// Returns a store error when the row cannot be read or decoded.
    pub fn reclamation_tombstone(&self, digest: &ArtifactDigest) -> Result<Option<ReclamationTombstone>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(RECLAMATION_TOMBSTONE)?;
        Ok(table
            .get(digest.canonical().as_str())?
            .map(|value| serde_json::from_slice(value.value()))
            .transpose()?)
    }

    /// List every reclamation tombstone in canonical digest order.
    ///
    /// # Errors
    /// Returns a store error when a row cannot be read or decoded.
    pub fn reclamation_tombstones(&self) -> Result<Vec<ReclamationTombstone>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(RECLAMATION_TOMBSTONE)?;
        let mut records = Vec::new();
        for entry in table.iter()? {
            let (_key, value) = entry?;
            records.push(serde_json::from_slice(value.value())?);
        }
        Ok(records)
    }

    /// Remove up to `limit` abandoned ([`Skipped`](ReclamationState::Skipped)) tombstones, bounding the
    /// terminal backlog a scheduler clears in one pass. Returns the number removed, so a caller loops
    /// until it returns fewer than `limit`.
    ///
    /// A `Skipped` tombstone is terminal: re-selecting the digest moves it back to
    /// [`Pending`](ReclamationState::Pending), so pruning one never races a live decision.
    ///
    /// # Errors
    /// Returns a store error when a row cannot be read, decoded, or committed.
    pub fn prune_skipped_reclamation_tombstones(&self, limit: usize) -> Result<usize, MetaError> {
        let txn = self.db.begin_write()?;
        let stale = {
            let table = txn.open_table(RECLAMATION_TOMBSTONE)?;
            let mut keys = Vec::new();
            for entry in table.iter()? {
                if keys.len() >= limit {
                    break;
                }
                let (raw_key, value) = entry?;
                let record: ReclamationTombstone = serde_json::from_slice(value.value())?;
                if matches!(record.state, ReclamationState::Skipped { .. }) {
                    keys.push(raw_key.value().to_owned());
                }
            }
            keys
        };
        let removed = stale.len();
        {
            let mut table = txn.open_table(RECLAMATION_TOMBSTONE)?;
            for key in stale {
                table.remove(key.as_str())?;
            }
        }
        txn.commit()?;
        Ok(removed)
    }

    /// Count reclamation tombstones by state for a bounded progress view.
    ///
    /// # Errors
    /// Returns a store error when a row cannot be read or decoded.
    pub fn reclamation_progress(&self) -> Result<ReclamationProgress, MetaError> {
        let mut progress = ReclamationProgress::default();
        for tombstone in self.reclamation_tombstones()? {
            match tombstone.state.status() {
                ReclamationStatus::Pending => progress.pending += 1,
                ReclamationStatus::Ready => progress.ready += 1,
                ReclamationStatus::Skipped => progress.skipped += 1,
            }
        }
        Ok(progress)
    }
}

fn terminal_skip(
    txn: &redb::WriteTransaction,
    digest: &ArtifactDigest,
    referenced: bool,
) -> Result<Option<SkipReason>, MetaError> {
    if referenced {
        return Ok(Some(SkipReason::Referenced));
    }
    if digest_is_serveable(txn, digest)? {
        return Ok(Some(SkipReason::Serveable));
    }
    Ok(None)
}

fn advance(
    prior: &ReclamationTombstone,
    state: ReclamationState,
    required_frontier: u64,
    fence: u64,
    now: i64,
) -> ReclamationTombstone {
    ReclamationTombstone {
        digest: prior.digest.clone(),
        state,
        required_frontier,
        fence: prior.fence.max(fence),
        attempts: prior.attempts + 1,
        selected_at_unix: prior.selected_at_unix,
        updated_at_unix: now,
    }
}

const fn guard_fence(existing: Option<&ReclamationTombstone>, fence: u64) -> Result<(), ReclamationError> {
    if let Some(record) = existing
        && fence < record.fence
    {
        return Err(ReclamationError::StaleFence {
            current: record.fence,
            applied: fence,
        });
    }
    Ok(())
}

fn read_tombstone(
    txn: &redb::WriteTransaction,
    digest: &ArtifactDigest,
) -> Result<Option<ReclamationTombstone>, MetaError> {
    let table = txn.open_table(RECLAMATION_TOMBSTONE)?;
    Ok(table
        .get(digest.canonical().as_str())?
        .map(|value| serde_json::from_slice(value.value()))
        .transpose()?)
}

fn write_tombstone(txn: &redb::WriteTransaction, record: &ReclamationTombstone) -> Result<(), MetaError> {
    let value = serde_json::to_vec(record)?;
    txn.open_table(RECLAMATION_TOMBSTONE)?
        .insert(record.digest.canonical().as_str(), value.as_slice())?;
    Ok(())
}

fn digest_is_serveable(txn: &redb::WriteTransaction, digest: &ArtifactDigest) -> Result<bool, MetaError> {
    let table = txn.open_table(BLOB_PLACEMENT)?;
    let (low, high) = BlobPlacementKey::digest_bounds(digest);
    for entry in table.range::<&str>((Included(low.as_str()), Excluded(high.as_str())))? {
        let (_key, value) = entry?;
        let record: BlobPlacementRecord = serde_json::from_slice(value.value())?;
        if matches!(record.state, BlobPlacementState::Verified { .. }) {
            return Ok(true);
        }
    }
    Ok(false)
}
