//! The pass demotes corrupt local copies and revokes verified copies outside the target data centers. It
//! skips integrity checks for digests under revocation or reclamation. The ledger is node-wide, so writes
//! use the cluster ownership term; term `0` disables reconciliation.
use std::collections::BTreeSet;
use std::num::NonZeroUsize;

use peryx_core::Clock;
use peryx_ha::{
    AvailabilityTaskError, AvailabilityTaskReport, BlobPlacementKey, BlobPlacementState, BlobPlacementTransition,
    DataCenterId, ReclamationState, ReclamationStore,
};
use peryx_identity::ArtifactDigest;
use peryx_storage::blob::{BlobErrorKind, BlobStore, Digest};
use peryx_storage::meta::{DigestRevocationState, MetaError, MetaStore};

use crate::placement_planning::out_of_policy_placements;
use crate::placement_policy::apply_blob_placement;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PassTally {
    scanned: u64,
    changed: u64,
}

pub struct FilesystemPlacementReconciler {
    local_dc: DataCenterId,
    store: BlobStore,
    target_dcs: BTreeSet<DataCenterId>,
}

impl FilesystemPlacementReconciler {
    #[must_use]
    pub fn new(local_dc: DataCenterId, store: BlobStore, target_dcs: BTreeSet<DataCenterId>) -> Option<Self> {
        if target_dcs.is_empty() {
            return None;
        }
        Some(Self {
            local_dc,
            store,
            target_dcs,
        })
    }

    fn verify_local_placements(
        &self,
        meta: &MetaStore,
        clock: &Clock,
        fence: u64,
        batch: NonZeroUsize,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<PassTally, AvailabilityTaskError> {
        let mut tally = PassTally::default();
        let mut cursor: Option<String> = None;
        loop {
            if cancelled() {
                break;
            }
            let page = meta
                .scan_blob_placements(cursor.as_deref(), batch)
                .map_err(|error| task_error("placement_verify_scan", error))?;
            for record in page.records.iter().filter(|record| {
                record.key.data_center == self.local_dc && matches!(record.state, BlobPlacementState::Verified { .. })
            }) {
                if cancelled() {
                    break;
                }
                tally.scanned += 1;
                let withdrawn = is_withdrawn(meta, &record.key.digest)
                    .map_err(|error| task_error("placement_withdrawal_read", error))?;
                if withdrawn {
                    continue;
                }
                if self.repair_if_corrupt(meta, clock, fence, &record.key) {
                    tally.changed += 1;
                }
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(tally)
    }

    fn repair_if_corrupt(&self, meta: &MetaStore, clock: &Clock, fence: u64, key: &BlobPlacementKey) -> bool {
        let digest = Digest::from_hex(key.digest.sha256()).expect("artifact digests are validated SHA-256");
        let intact = match self.store.verify(&digest) {
            Ok(intact) => intact,
            // Missing files and corrupt bytes both invalidate verified placements.
            Err(error) if error.kind() == BlobErrorKind::NotFound => false,
            Err(error) => {
                tracing::warn!(%error, "placement reconcile could not re-verify a stored blob");
                return false;
            }
        };
        if intact {
            return false;
        }
        // Stop routing to corrupt bytes before clearing the path for a replacement.
        if !record_transition(meta, key, &BlobPlacementTransition::Invalidate, fence, clock) {
            return false;
        }
        if let Err(error) = self.store.remove(&digest) {
            tracing::warn!(%error, "placement reconcile could not remove a corrupt blob");
        }
        true
    }

    fn retire_out_of_policy(
        &self,
        meta: &MetaStore,
        clock: &Clock,
        fence: u64,
        batch: NonZeroUsize,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<PassTally, AvailabilityTaskError> {
        let mut tally = PassTally::default();
        let mut cursor: Option<String> = None;
        loop {
            if cancelled() {
                break;
            }
            // Revoke cannot make a withdrawn digest serveable, so retirement needs no withdrawal lookup.
            let page = meta
                .scan_blob_placement_groups(cursor.as_deref(), batch)
                .map_err(|error| task_error("placement_reconcile_scan", error))?;
            tally.scanned += page.groups.len() as u64;
            for records in &page.groups {
                for key in out_of_policy_placements(records, &self.target_dcs) {
                    if record_transition(meta, &key, &BlobPlacementTransition::Revoke, fence, clock) {
                        tally.changed += 1;
                    }
                }
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(tally)
    }
}

impl FilesystemPlacementReconciler {
    /// # Errors
    /// Returns an error when placement or withdrawal metadata access fails.
    pub fn reconcile_pass(
        &self,
        meta: &MetaStore,
        clock: &Clock,
        fence: u64,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
        batch: NonZeroUsize,
    ) -> Result<AvailabilityTaskReport, AvailabilityTaskError> {
        if fence == 0 {
            return Ok(AvailabilityTaskReport::default());
        }
        let integrity = self.verify_local_placements(meta, clock, fence, batch, cancelled)?;
        let policy = self.retire_out_of_policy(meta, clock, fence, batch, cancelled)?;
        Ok(AvailabilityTaskReport {
            processed: integrity.scanned + policy.scanned,
            changed: integrity.changed + policy.changed,
        })
    }
}

fn task_error(code: &'static str, error: impl std::fmt::Display) -> AvailabilityTaskError {
    AvailabilityTaskError::new(code, error.to_string())
}

/// Integrity checks leave digests under revocation or reclamation alone. Retirement applies only
/// `Revoke`, so it cannot make a digest serveable.
///
/// # Errors
/// Returns [`MetaError`] when a revocation or reclamation lookup fails.
fn is_withdrawn(meta: &MetaStore, digest: &ArtifactDigest) -> Result<bool, MetaError> {
    if meta
        .digest_revocation(digest)?
        .is_some_and(|revocation| matches!(revocation.state, DigestRevocationState::Active))
    {
        return Ok(true);
    }
    Ok(meta
        .reclamation_tombstone(digest)?
        .is_some_and(|tombstone| matches!(tombstone.state, ReclamationState::Pending | ReclamationState::Ready)))
}

fn record_transition(
    meta: &MetaStore,
    key: &BlobPlacementKey,
    transition: &BlobPlacementTransition,
    fence: u64,
    clock: &Clock,
) -> bool {
    match apply_blob_placement(meta, key, transition, fence, (clock)()) {
        Ok(_) => true,
        Err(error) => {
            tracing::warn!(%error, ?transition, "placement reconcile could not record a placement");
            false
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/placement_runtime_tests.rs"]
mod tests;
