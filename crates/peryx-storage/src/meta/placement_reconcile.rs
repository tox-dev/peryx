//! Reconciling the cross-data-center placement ledger against a desired replication policy.
//!
//! Where [`super::cross_dc_copy`] answers "what does the local data center owe," this module takes a
//! whole-fleet view: given the set of data centers that should each hold a verified copy of a digest,
//! it reads the [`BlobPlacementRecord`] ledger and reports where reality diverges. A target data center
//! with no verified copy needs one placed; a verified copy in a data center outside the policy is a
//! retirement candidate.
//!
//! Reconciliation never resurrects withdrawn content. The caller injects a predicate over the
//! revocation and reclamation records, exactly as [`MetaStore::repair_artifact_placements`] injects its
//! presence probe, and a withdrawn digest is passed over rather than scheduled for a copy. Detecting a
//! rotted local file and executing the repairs and retirements live above this module, which only reads
//! the ledger and classifies.

use std::collections::BTreeSet;
use std::ops::Bound::{Excluded, Included, Unbounded};

use peryx_identity::ArtifactDigest;
use redb::ReadableTable as _;

use super::{
    BLOB_PLACEMENT, BlobPlacementKey, BlobPlacementRecord, BlobPlacementState, DataCenterId, MetaError, MetaStore,
};

/// The most placement groups one reconciliation pass may read, bounding the work a single pass does.
pub const MAX_PLACEMENT_RECONCILE_BATCH: usize = 1_000;

/// How one digest's placements diverge from the replication policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestReconciliation {
    pub digest: ArtifactDigest,
    /// Target data centers that hold no verified copy, in policy order.
    pub replicate: Vec<DataCenterId>,
    /// Verified placements in data centers outside the policy, in ledger order.
    pub retire: Vec<BlobPlacementKey>,
}

/// One bounded scan of the local data center's verified placements, in ledger order, with a cursor to
/// resume. The integrity pass re-verifies each returned placement's stored bytes against its digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalVerifiedPlacementPage {
    /// The verified placements the local data center holds among the rows this page read.
    pub placements: Vec<BlobPlacementRecord>,
    /// How many ledger rows the pass read, whatever their data center or state.
    pub scanned: usize,
    /// The encoded ledger key to resume after, present only when more rows remain past this page.
    pub next_cursor: Option<String>,
}

/// One bounded reconciliation pass in digest order, with a cursor to resume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementReconcilePage {
    /// Only digests that diverge from the policy, so a converged fleet yields an empty page.
    pub reconciliations: Vec<DigestReconciliation>,
    /// How many distinct digests the pass classified.
    pub scanned: usize,
    /// The digest to resume after, present only when more digests remain past this page.
    pub next_cursor: Option<String>,
}

/// A rejected reconciliation pass.
#[derive(Debug, thiserror::Error)]
pub enum PlacementReconcileError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("limit must be between 1 and {MAX_PLACEMENT_RECONCILE_BATCH}")]
    InvalidLimit,
}

/// Classify one digest's placements against the target data centers.
fn reconcile_digest(records: &[BlobPlacementRecord], target_dcs: &BTreeSet<DataCenterId>) -> DigestReconciliation {
    let mut verified_dcs = BTreeSet::new();
    let mut retire = Vec::new();
    for record in records {
        if matches!(record.state, BlobPlacementState::Verified { .. }) {
            verified_dcs.insert(record.key.data_center.clone());
            if !target_dcs.contains(&record.key.data_center) {
                retire.push(record.key.clone());
            }
        }
    }
    let replicate = target_dcs
        .iter()
        .filter(|dc| !verified_dcs.contains(*dc))
        .cloned()
        .collect();
    DigestReconciliation {
        digest: records[0].key.digest.clone(),
        replicate,
        retire,
    }
}

impl MetaStore {
    /// Scan the placement ledger in digest order for divergence from the replication policy, resuming
    /// after `cursor` and reading at most `limit` distinct digests.
    ///
    /// `withdrawn` is consulted once per digest; a digest it accepts is passed over so reconciliation
    /// never schedules a copy for revoked or reclaimed content. The pass bounds work by digest count
    /// and carries a `next_cursor` at its last digest until it reaches the end of the table.
    ///
    /// # Errors
    /// Returns [`PlacementReconcileError::InvalidLimit`] for a limit outside
    /// `1..=MAX_PLACEMENT_RECONCILE_BATCH`, or a store error when a row cannot be read or decoded.
    pub fn reconcile_placement_policy(
        &self,
        target_dcs: &BTreeSet<DataCenterId>,
        cursor: Option<&str>,
        limit: usize,
        mut withdrawn: impl FnMut(&ArtifactDigest) -> bool,
    ) -> Result<PlacementReconcilePage, PlacementReconcileError> {
        if !(1..=MAX_PLACEMENT_RECONCILE_BATCH).contains(&limit) {
            return Err(PlacementReconcileError::InvalidLimit);
        }
        let txn = self.db.begin_read().map_err(MetaError::from)?;
        let table = txn.open_table(BLOB_PLACEMENT).map_err(MetaError::from)?;
        let resume = cursor.map(|digest| format!("{digest}\u{1}"));
        let entries_iter = resume
            .as_ref()
            .map_or_else(
                || table.iter(),
                |bound| table.range::<&str>((Included(bound.as_str()), Unbounded)),
            )
            .map_err(MetaError::from)?;

        let mut page = PlacementReconcilePage {
            reconciliations: Vec::new(),
            scanned: 0,
            next_cursor: None,
        };
        let mut group: Vec<BlobPlacementRecord> = Vec::new();
        for entry in entries_iter {
            let (_key, value) = entry.map_err(MetaError::from)?;
            let record: BlobPlacementRecord = serde_json::from_slice(value.value()).map_err(MetaError::from)?;
            if group.first().is_some_and(|first| first.key.digest != record.key.digest) {
                fold_group(&mut page, &group, target_dcs, &mut withdrawn);
                if page.scanned == limit {
                    page.next_cursor = Some(group[0].key.digest.canonical());
                    return Ok(page);
                }
                group.clear();
            }
            group.push(record);
        }
        if !group.is_empty() {
            fold_group(&mut page, &group, target_dcs, &mut withdrawn);
        }
        Ok(page)
    }

    /// Scan the placement ledger in key order for the local data center's verified placements, resuming
    /// after `cursor` and reading at most `limit` rows.
    ///
    /// The integrity pass re-verifies each returned placement's stored bytes; a mismatch demotes the
    /// verified copy so the copy backlog repairs it. The pass bounds work by rows read and carries a
    /// `next_cursor` at its last read row until it reaches the end of the table, so a ledger
    /// dominated by remote or non-verified rows still advances a bounded amount per pass.
    ///
    /// # Errors
    /// Returns [`PlacementReconcileError::InvalidLimit`] for a limit outside
    /// `1..=MAX_PLACEMENT_RECONCILE_BATCH`, or a store error when a row cannot be read or decoded.
    pub fn scan_local_verified_placements(
        &self,
        local_dc: &DataCenterId,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<LocalVerifiedPlacementPage, PlacementReconcileError> {
        if !(1..=MAX_PLACEMENT_RECONCILE_BATCH).contains(&limit) {
            return Err(PlacementReconcileError::InvalidLimit);
        }
        let txn = self.db.begin_read().map_err(MetaError::from)?;
        let table = txn.open_table(BLOB_PLACEMENT).map_err(MetaError::from)?;
        let entries_iter = cursor
            .map_or_else(
                || table.iter(),
                |after| table.range::<&str>((Excluded(after), Unbounded)),
            )
            .map_err(MetaError::from)?;

        let mut page = LocalVerifiedPlacementPage {
            placements: Vec::new(),
            scanned: 0,
            next_cursor: None,
        };
        for entry in entries_iter {
            let (key, value) = entry.map_err(MetaError::from)?;
            page.scanned += 1;
            let record: BlobPlacementRecord = serde_json::from_slice(value.value()).map_err(MetaError::from)?;
            if record.key.data_center == *local_dc && matches!(record.state, BlobPlacementState::Verified { .. }) {
                page.placements.push(record);
            }
            if page.scanned == limit {
                page.next_cursor = Some(key.value().to_owned());
                return Ok(page);
            }
        }
        Ok(page)
    }
}

/// Fold one completed digest group into the page: unless it is withdrawn, record its divergence, and
/// count it either way.
fn fold_group(
    page: &mut PlacementReconcilePage,
    group: &[BlobPlacementRecord],
    target_dcs: &BTreeSet<DataCenterId>,
    withdrawn: &mut impl FnMut(&ArtifactDigest) -> bool,
) {
    page.scanned += 1;
    if withdrawn(&group[0].key.digest) {
        return;
    }
    let reconciliation = reconcile_digest(group, target_dcs);
    if !reconciliation.replicate.is_empty() || !reconciliation.retire.is_empty() {
        page.reconciliations.push(reconciliation);
    }
}
