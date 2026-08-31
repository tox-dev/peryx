use std::num::NonZeroUsize;

use peryx_identity::ArtifactDigest;
use serde::{Deserialize, Serialize};

use crate::{
    ArtifactPlacement, ArtifactPlacementHealth, ArtifactPlacementPage, ArtifactPlacementQuery, BlobPlacementGroupPage,
    BlobPlacementKey, BlobPlacementPage, BlobPlacementRecord, NewReconcileEntry, ReclaimGuard, ReclaimGuardArm,
    ReclamationSnapshot, ReclamationTombstone, ReclamationTombstonePage, ReconcileEnqueue, ReconcileEntry,
    ReconcilePage,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareWrite {
    Written,
    Conflict,
    CapacityExceeded,
}

/// A reclamation write carries the reference revision its referenced verdict was proved against, so a
/// digest that gained a reference after the proof cannot be written as unreferenced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TombstoneWrite {
    Written,
    Conflict,
    ReferencesMoved,
}

#[expect(clippy::missing_errors_doc, reason = "implementations define backend errors")]
pub trait BlobPlacementStore {
    type Error;

    fn blob_placement(&self, key: &BlobPlacementKey) -> Result<Option<BlobPlacementRecord>, Self::Error>;
    fn blob_placements(&self, digest: &ArtifactDigest) -> Result<Vec<BlobPlacementRecord>, Self::Error>;
    fn scan_blob_placements(&self, cursor: Option<&str>, limit: NonZeroUsize)
    -> Result<BlobPlacementPage, Self::Error>;
    fn scan_blob_placement_groups(
        &self,
        cursor: Option<&str>,
        limit: NonZeroUsize,
    ) -> Result<BlobPlacementGroupPage, Self::Error>;
    fn compare_and_put_blob_placement(
        &self,
        expected: Option<&BlobPlacementRecord>,
        replacement: &BlobPlacementRecord,
    ) -> Result<CompareWrite, Self::Error>;
}

#[expect(clippy::missing_errors_doc, reason = "implementations define backend errors")]
pub trait ArtifactPlacementStore {
    type Error;
    type QueryError;

    fn put_artifact_placement(&self, digest: &str, placement: &ArtifactPlacement) -> Result<(), Self::Error>;
    fn get_artifact_placement(&self, digest: &str) -> Result<Option<ArtifactPlacement>, Self::Error>;
    fn insert_artifact_placement(
        &self,
        digest: &str,
        placement: &ArtifactPlacement,
    ) -> Result<ArtifactPlacement, Self::Error>;
    fn compare_and_put_artifact_placement(
        &self,
        digest: &str,
        expected: &ArtifactPlacement,
        replacement: &ArtifactPlacement,
    ) -> Result<bool, Self::Error>;
    fn delete_artifact_placement(&self, digest: &str) -> Result<bool, Self::Error>;
    fn list_artifact_placements(
        &self,
        query: &ArtifactPlacementQuery,
    ) -> Result<ArtifactPlacementPage, Self::QueryError>;
    fn artifact_placement_health(&self) -> Result<ArtifactPlacementHealth, Self::Error>;
}

#[expect(clippy::missing_errors_doc, reason = "implementations define backend errors")]
pub trait ReclamationStore {
    type Error;

    fn reclamation_snapshot(&self, digest: &ArtifactDigest) -> Result<ReclamationSnapshot, Self::Error>;
    /// Writes only while the store's reference revision still equals `revision`.
    fn compare_and_put_reclamation_tombstone(
        &self,
        expected: &ReclamationSnapshot,
        replacement: &ReclamationTombstone,
        revision: u64,
    ) -> Result<TombstoneWrite, Self::Error>;
    fn compare_and_remove_reclamation_tombstone(&self, expected: &ReclamationTombstone) -> Result<bool, Self::Error>;
    fn reclamation_tombstone(&self, digest: &ArtifactDigest) -> Result<Option<ReclamationTombstone>, Self::Error>;
    fn reclamation_tombstones(&self) -> Result<Vec<ReclamationTombstone>, Self::Error>;
    /// Returns the last emitted digest as a cursor only when another row exists; passing it back starts at the
    /// following tombstone.
    fn scan_reclamation_tombstones(
        &self,
        cursor: Option<&str>,
        limit: NonZeroUsize,
    ) -> Result<ReclamationTombstonePage, Self::Error>;
}

#[expect(clippy::missing_errors_doc, reason = "implementations define backend errors")]
pub trait ReclaimGuardStore {
    type Error;

    /// Arms only while the store's reference revision still equals `revision`.
    fn compare_and_arm_reclaim_guards(
        &self,
        digests: &[&str],
        revision: u64,
        now: i64,
        replacement: ReclaimGuard,
    ) -> Result<ReclaimGuardArm, Self::Error>;
    fn compare_and_disarm_reclaim_guard(&self, digest: &str, expected: ReclaimGuard) -> Result<bool, Self::Error>;
    fn reclaim_guard(&self, digest: &str) -> Result<Option<ReclaimGuard>, Self::Error>;
    fn reclaim_guards(&self) -> Result<Vec<(String, ReclaimGuard)>, Self::Error>;
}

#[expect(clippy::missing_errors_doc, reason = "implementations define backend errors")]
pub trait ReconcileStore {
    type Error;

    fn enqueue_reconcile(&self, entry: &NewReconcileEntry<'_>, now: i64) -> Result<ReconcileEnqueue, Self::Error>;
    fn pending_reconcile(&self, limit: usize) -> Result<Vec<(String, ReconcileEntry)>, Self::Error>;
    fn settled_reconcile(&self, limit: usize) -> Result<Vec<(String, ReconcileEntry)>, Self::Error>;
    /// Returns the last emitted key as a cursor only when another row exists; passing it back starts at the
    /// following row.
    fn scan_reconcile(&self, cursor: Option<&str>, limit: NonZeroUsize) -> Result<ReconcilePage, Self::Error>;
    fn settle_reconcile(&self, key: &str, outcome: &str, now: i64) -> Result<bool, Self::Error>;
    fn compare_and_remove_reconcile(&self, key: &str, expected: &ReconcileEntry) -> Result<bool, Self::Error>;
    fn reconcile_entry(&self, key: &str) -> Result<Option<ReconcileEntry>, Self::Error>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferAudit {
    pub authority: String,
    pub source: String,
    pub target: String,
    pub actor: String,
    pub reason: String,
    pub barrier: u64,
    pub epoch: u64,
    pub commit_index: u64,
}

#[expect(clippy::missing_errors_doc, reason = "implementations define backend errors")]
pub trait TransferAuditStore {
    type Error;

    fn record_transfer_audit(&self, audit: &TransferAudit) -> Result<(), Self::Error>;
    fn transfer_audits(&self, authority: &str) -> Result<Vec<TransferAudit>, Self::Error>;
}
