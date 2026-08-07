//! The background blob-reclamation selector.
//!
//! The single-node cache purge decides a blob is unreferenced from one snapshot and deletes it. In a
//! replicated topology that is unsafe: a lagging replica or backup can still be replaying metadata that
//! names the blob, so deleting its bytes now would strand a plane that has not caught up. This is the
//! orchestrator behind the scheduled [`Reclamation`](peryx_driver::jobs::ScheduledJob::Reclamation) job.
//! It drives the durable [reclamation tombstone](peryx_storage::meta::ReclamationTombstone) storage core
//! through two bounded phases and **deletes no bytes** - the backend-delete executor is a later concern.
//!
//! - **Select.** Walk the stored blobs, and for each digest the reference inventory does not name, arm a
//!   pending tombstone at the current metadata serial as its required frontier. The storage core rejects
//!   a digest a verified placement can still serve, so a replica that still needs the bytes blocks
//!   selection atomically.
//! - **Finalize.** For each pending tombstone every replication plane has advanced past whose references
//!   have not returned, mark it ready under the fence - the final reference and serveability re-check the
//!   storage core runs before a candidate becomes eligible for deletion.
//!
//! # Fencing
//!
//! Reclamation decides destructive storage state, so exactly one node runs it cluster-wide: the scheduler
//! leases it under the ownership group's monotonic term through
//! [`ClusterSingleton`](peryx_driver::jobs::LeaseScope::ClusterSingleton), and the pass stamps that term
//! as each tombstone's fence. A partitioned former holder mints a stale term and its writes are rejected,
//! and a process running no ownership group reads term `0` and reclaims nothing.
//!
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;

use peryx_driver::jobs::{JobFailure, JobReport, ReclamationParameters};
use peryx_driver::state::ServingState;
use peryx_ha::{AvailabilityTaskError, AvailabilityTaskReport, ReclamationFrontiers, ReferenceInventory};
use peryx_identity::ArtifactDigest;
use peryx_storage::meta::{ObservedFrontier, ReadyOutcome, ReclamationState, SelectOutcome};

/// The background blob-reclamation selector registered on the [`ServingState`].
pub struct BlobReclamationSelector {
    references: Arc<dyn ReferenceInventory>,
    frontiers: Arc<dyn ReclamationFrontiers>,
}

struct BoundBlobReclaimer {
    reclaimer: BlobReclamationSelector,
    state: Arc<ServingState>,
}

impl BlobReclamationSelector {
    pub fn bind(self, state: Arc<ServingState>) -> Arc<dyn peryx_ha::BlobReclaimer> {
        Arc::new(BoundBlobReclaimer { reclaimer: self, state })
    }
    #[must_use]
    pub fn new(references: Arc<dyn ReferenceInventory>, frontiers: Arc<dyn ReclamationFrontiers>) -> Self {
        Self { references, frontiers }
    }
}

impl BlobReclamationSelector {
    pub(crate) fn reclaim_pass(
        &self,
        state: &ServingState,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
        fence: u64,
        params: ReclamationParameters,
    ) -> Result<JobReport, JobFailure> {
        // A process running no ownership group reads term 0; it holds no cluster-singleton lease and
        // reclaims nothing, mirroring the copy pass.
        if fence == 0 {
            return Ok(JobReport::default());
        }
        let meta = &state.meta;
        let now = (state.clock)();
        let referenced = match self.references.referenced(meta) {
            Ok(referenced) => referenced,
            Err(reason) => return Err(JobFailure::new("reclamation_references", reason)),
        };
        let required = match meta.current_serial() {
            Ok(required) => required,
            Err(error) => return Err(JobFailure::new("reclamation_frontier_read", error.to_string())),
        };
        let mut report = JobReport::default();

        // Select: arm a pending tombstone for each unreferenced, unservable stored digest, bounded by the
        // batch so one pass reads a bounded slice of the ledger.
        let mut stored = Vec::new();
        let scan = state.blobs.blocking().visit(|entry| {
            if let Some(digest) = entry.digest {
                stored.push(digest);
            }
            Ok::<(), Infallible>(())
        });
        if let Err(error) = scan {
            return Err(JobFailure::new("reclamation_scan", error.to_string()));
        }
        for digest in stored.into_iter().take(params.batch.get()) {
            if cancelled() {
                return Ok(report);
            }
            report.processed += 1;
            if referenced.contains(digest.as_str()) {
                continue;
            }
            let artifact = ArtifactDigest::from_sha256(digest.as_str())
                .expect("blob storage only yields canonical SHA-256 digests");
            match meta.select_reclamation_candidate(&artifact, false, required, fence, now) {
                Ok(SelectOutcome::Selected(_)) => report.changed += 1,
                Ok(_) => {}
                Err(error) => return Err(JobFailure::new("reclamation_select", error.to_string())),
            }
        }

        // Finalize: mark ready each pending tombstone both planes have cleared whose references have not
        // returned - the storage core's final reference and serveability re-check under the fence.
        let Some(observed) = self.frontiers.observe() else {
            return Ok(report);
        };
        let tombstones = match meta.reclamation_tombstones() {
            Ok(tombstones) => tombstones,
            Err(error) => return Err(JobFailure::new("reclamation_read", error.to_string())),
        };
        for tombstone in tombstones {
            if cancelled() {
                return Ok(report);
            }
            if !matches!(tombstone.state, ReclamationState::Pending) {
                continue;
            }
            let referenced_now = referenced.contains(tombstone.digest.sha256());
            match meta.mark_reclamation_ready(&tombstone.digest, referenced_now, observed, fence, now) {
                Ok(ReadyOutcome::Ready(_)) => report.changed += 1,
                Ok(_) => {}
                Err(error) => return Err(JobFailure::new("reclamation_mark", error.to_string())),
            }
        }
        Ok(report)
    }
}

#[async_trait]
impl peryx_ha::BlobReclaimer for BoundBlobReclaimer {
    async fn reclaim_pass(
        &self,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
        fence: u64,
        batch: std::num::NonZeroUsize,
    ) -> Result<AvailabilityTaskReport, AvailabilityTaskError> {
        self.reclaimer
            .reclaim_pass(&self.state, cancelled, fence, ReclamationParameters { batch })
            .map(|report| AvailabilityTaskReport {
                processed: report.processed,
                changed: report.changed,
            })
            .map_err(|error| AvailabilityTaskError::new(error.code(), error.message()))
    }
}

#[cfg(test)]
#[path = "reclamation_runtime_tests.rs"]
mod tests;

pub struct ReplicaReclamationFrontiers {
    liveness: Option<Arc<crate::LivenessTracker>>,
    replicas: Vec<String>,
}

impl ReplicaReclamationFrontiers {
    #[must_use]
    pub const fn new(liveness: Option<Arc<crate::LivenessTracker>>, replicas: Vec<String>) -> Self {
        Self { liveness, replicas }
    }
}

impl ReclamationFrontiers for ReplicaReclamationFrontiers {
    fn observe(&self) -> Option<ObservedFrontier> {
        if self.replicas.is_empty() {
            return Some(ObservedFrontier {
                replica: None,
                backup: None,
            });
        }
        let liveness = self.liveness.as_ref()?;
        let now = Instant::now();
        let replica = self
            .replicas
            .iter()
            .map(|node| liveness.applied_frontier(node, now))
            .collect::<Option<Vec<_>>>()?
            .into_iter()
            .min();
        Some(ObservedFrontier { replica, backup: None })
    }
}
