//! The background filesystem-placement reconciler.
//!
//! Local-filesystem HA needs each data center's stored blobs to match the replication policy: every
//! target data center holds a verified copy of each live digest, no out-of-policy copies linger, and a
//! stored copy whose bytes have rotted is caught and repaired. This is the orchestrator behind the
//! scheduled [`PlacementReconcile`](peryx_driver::jobs::ScheduledJob::PlacementReconcile) job.
//!
//! It runs two bounded passes over the placement ledger, both fenced and both skipping any digest a
//! revocation or an in-flight reclamation has withdrawn, so reconciliation never resurrects deleted
//! content or races garbage collection:
//!
//! - **Integrity.** It re-verifies the local data center's verified placements against their stored
//!   bytes. A copy that fails - its bytes rotted, or its file vanished under a live record - demotes to a
//!   digest-mismatch failure and its bad bytes are dropped, so it leaves the served set and the copy
//!   backlog schedules a fresh copy from a verified peer. Detecting the rot is this job's; the repair copy
//!   is the [`DcCopy`](peryx_driver::jobs::ScheduledJob::DcCopy) job's.
//! - **Policy.** It classifies each digest's placements against the target data centers and retires the
//!   verified copies that sit outside the policy - a data center dropped from membership, say - by
//!   revoking them from serving. A target data center that lacks a copy is that data center's copy backlog
//!   to fill, not this pass's.
//!
//! # Fencing
//!
//! Like the copy pass, reconciliation is fenced by the ownership group's cluster-level monotonic epoch,
//! [`ClusterStatus::term`], not a per-repository authority epoch: the ledger is digest-keyed and node-wide,
//! so it names no repository. A process running no ownership group reads term `0` and reconciles nothing.
//!
//! [`ClusterStatus::term`]: peryx_driver::state::ClusterStatus::term

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;

use peryx_driver::jobs::{JobFailure, JobReport, PlacementReconcileParameters};
use peryx_driver::state::{Clock, ServingState};
use peryx_ha::{AvailabilityTaskError, AvailabilityTaskReport};
use peryx_identity::ArtifactDigest;
use peryx_storage::blob::{BlobErrorKind, BlobStore, Digest};
use peryx_storage::meta::{
    BlobPlacementFailure, BlobPlacementKey, BlobPlacementTransition, DataCenterId, DigestRevocationState, MetaError,
    MetaStore, ReclamationState,
};

/// The tallies one bounded pass contributes to the job report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct PassTally {
    scanned: u64,
    changed: u64,
}

/// The background filesystem-placement reconciler registered on the [`ServingState`].
pub struct FilesystemPlacementReconciler {
    /// The data center this node re-verifies stored copies for.
    local_dc: DataCenterId,
    /// The local filesystem store whose bytes the integrity pass re-hashes.
    store: BlobStore,
    /// The data centers the replication policy targets: each should hold a verified copy of every live
    /// digest, so a verified copy outside this set is a retirement candidate.
    target_dcs: BTreeSet<DataCenterId>,
}

struct BoundPlacementReconciler {
    reconciler: FilesystemPlacementReconciler,
    state: Arc<ServingState>,
}

impl FilesystemPlacementReconciler {
    pub fn bind(self, state: Arc<ServingState>) -> Arc<dyn peryx_ha::PlacementReconciler> {
        Arc::new(BoundPlacementReconciler {
            reconciler: self,
            state,
        })
    }
    /// Build the reconciler for a filesystem node from its configuration and runtime store.
    ///
    /// Returns `None` when the node reconciles nothing: no membership, no node identity, this node
    /// absent from the roster, or a single-data-center group with no peer a repair could pull from.
    ///
    /// The node names its own roster entry through `node_identity`, so the reconciler resolves its own
    /// datacenter from that; `writer_identity` is the shared writer every node claims and is the same
    /// across the group, so it names one datacenter for all and would place a replica in the authority's
    /// datacenter. It falls back to `writer_identity` only for a single-writer group that configures no
    /// distinct node identity.
    ///
    /// # Errors
    /// Returns an error when a configured data center is not a valid placement component.
    #[must_use]
    pub fn new(local_dc: DataCenterId, store: BlobStore, target_dcs: BTreeSet<DataCenterId>) -> Option<Self> {
        if target_dcs.len() < 2 {
            return None;
        }
        Some(Self {
            local_dc,
            store,
            target_dcs,
        })
    }

    /// Re-verify the local data center's verified placements, demoting each whose stored bytes no longer
    /// match its digest so the copy backlog repairs it.
    fn verify_local_placements(
        &self,
        meta: &MetaStore,
        clock: &Clock,
        fence: u64,
        batch: usize,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<PassTally, JobFailure> {
        let mut tally = PassTally::default();
        let mut cursor: Option<String> = None;
        loop {
            if cancelled() {
                break;
            }
            let page = meta
                .scan_local_verified_placements(&self.local_dc, cursor.as_deref(), batch)
                .map_err(|error| JobFailure::new("placement_verify_scan", error.to_string()))?;
            for record in &page.placements {
                if cancelled() {
                    break;
                }
                tally.scanned += 1;
                let withdrawn = is_withdrawn(meta, &record.key.digest)
                    .map_err(|error| JobFailure::new("placement_withdrawal_read", error.to_string()))?;
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

    /// Demote a verified placement whose stored bytes no longer verify and drop the bad bytes, reporting
    /// whether it did. An intact copy, or one whose demotion the fence rejects, is left as it is.
    fn repair_if_corrupt(&self, meta: &MetaStore, clock: &Clock, fence: u64, key: &BlobPlacementKey) -> bool {
        let digest = Digest::from(&key.digest);
        let intact = match self.store.verify(&digest) {
            Ok(intact) => intact,
            // A verified record over a vanished file is as broken as a rotted one, and repairs the same way.
            Err(error) if error.kind() == BlobErrorKind::NotFound => false,
            Err(error) => {
                tracing::warn!(%error, "placement reconcile could not re-verify a stored blob");
                return false;
            }
        };
        if intact {
            return false;
        }
        // Demote first, so routing stops serving the corrupt copy, then drop the bad bytes so the backlog
        // can publish a fresh copy over the now-empty path.
        if !record_transition(
            meta,
            key,
            &BlobPlacementTransition::Fail {
                class: BlobPlacementFailure::DigestMismatch,
            },
            fence,
            clock,
        ) {
            return false;
        }
        if let Err(error) = self.store.remove(&digest) {
            tracing::warn!(%error, "placement reconcile could not remove a corrupt blob");
        }
        true
    }

    /// Classify each digest against the target data centers and retire the verified copies that sit
    /// outside the policy by revoking them from serving.
    fn retire_out_of_policy(
        &self,
        meta: &MetaStore,
        clock: &Clock,
        fence: u64,
        batch: usize,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<PassTally, JobFailure> {
        let mut tally = PassTally::default();
        let mut cursor: Option<String> = None;
        loop {
            if cancelled() {
                break;
            }
            // Retiring an out-of-policy copy is a removal, which never resurrects withdrawn content, so the
            // retire pass classifies every digest - a withdrawn one included - rather than skip it.
            let page = meta
                .reconcile_placement_policy(&self.target_dcs, cursor.as_deref(), batch, |_digest| false)
                .map_err(|error| JobFailure::new("placement_reconcile_scan", error.to_string()))?;
            tally.scanned += page.scanned as u64;
            for reconciliation in &page.reconciliations {
                for key in &reconciliation.retire {
                    if record_transition(meta, key, &BlobPlacementTransition::Revoke, fence, clock) {
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
    pub(crate) fn reconcile_pass(
        &self,
        state: &ServingState,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
        params: PlacementReconcileParameters,
    ) -> Result<JobReport, JobFailure> {
        let fence = state
            .ownership_authority()
            .map_or(0, |authority| authority.cluster_status().term);
        if fence == 0 {
            return Ok(JobReport::default());
        }
        let meta = &state.meta;
        let clock = &state.clock;
        let integrity = self.verify_local_placements(meta, clock, fence, params.batch.get(), cancelled)?;
        let policy = self.retire_out_of_policy(meta, clock, fence, params.batch.get(), cancelled)?;
        Ok(JobReport {
            processed: integrity.scanned + policy.scanned,
            changed: integrity.changed + policy.changed,
        })
    }
}

#[async_trait]
impl peryx_ha::PlacementReconciler for BoundPlacementReconciler {
    async fn reconcile_pass(
        &self,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
        batch: std::num::NonZeroUsize,
    ) -> Result<AvailabilityTaskReport, AvailabilityTaskError> {
        self.reconciler
            .reconcile_pass(&self.state, cancelled, PlacementReconcileParameters { batch })
            .map(|report| AvailabilityTaskReport {
                processed: report.processed,
                changed: report.changed,
            })
            .map_err(task_error)
    }
}

fn task_error(error: JobFailure) -> AvailabilityTaskError {
    let (code, message) = error.into_parts();
    AvailabilityTaskError::new(code, message)
}

/// Whether repairing `digest` would resurrect withdrawn content: an active revocation has retired it, or
/// an in-flight reclamation is deleting it, so re-copying it would fight garbage collection.
///
/// Only the repair path consults this; retiring an out-of-policy copy is a removal that never resurrects,
/// so it needs no gate.
///
/// # Errors
/// Returns [`MetaError`] when a revocation or reclamation record cannot be read.
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

/// Apply one placement transition under `fence`, reporting whether it committed. A rejected transition is
/// logged and counted as not recorded rather than aborting the pass.
fn record_transition(
    meta: &MetaStore,
    key: &BlobPlacementKey,
    transition: &BlobPlacementTransition,
    fence: u64,
    clock: &Clock,
) -> bool {
    match meta.apply_blob_placement(key, transition, fence, (clock)()) {
        Ok(_) => true,
        Err(error) => {
            tracing::warn!(%error, ?transition, "placement reconcile could not record a placement");
            false
        }
    }
}

#[cfg(test)]
#[path = "placement_runtime_tests.rs"]
mod tests;
