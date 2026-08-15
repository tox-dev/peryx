use std::convert::Infallible;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;

use peryx_core::Clock;
use peryx_ha::{
    AvailabilityTaskError, AvailabilityTaskReport, ObservedFrontier, ReadyOutcome, ReclamationFrontiers,
    ReclamationState, ReclamationStore, ReferenceInventory, SelectOutcome,
};
use peryx_identity::ArtifactDigest;
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::MetaStore;

mod candidate_policy;
mod fence_policy;
mod retention_policy;

pub use candidate_policy::{ReclamationError, mark_reclamation_ready, select_reclamation_candidate};
pub use fence_policy::forget_reclamation_tombstone;
pub use retention_policy::{prune_skipped_reclamation_tombstones, reclamation_progress};

pub struct BlobReclamationSelector {
    references: Arc<dyn ReferenceInventory>,
    frontiers: Arc<dyn ReclamationFrontiers>,
}

struct BoundBlobReclaimer {
    reclaimer: BlobReclamationSelector,
    meta: MetaStore,
    blobs: BlobStorage,
    clock: Clock,
}

impl BlobReclamationSelector {
    pub fn bind(self, meta: MetaStore, blobs: BlobStorage, clock: Clock) -> Arc<dyn peryx_ha::BlobReclaimer> {
        Arc::new(BoundBlobReclaimer {
            reclaimer: self,
            meta,
            blobs,
            clock,
        })
    }
    #[must_use]
    pub fn new(references: Arc<dyn ReferenceInventory>, frontiers: Arc<dyn ReclamationFrontiers>) -> Self {
        Self { references, frontiers }
    }
}

impl BlobReclamationSelector {
    /// # Errors
    /// Returns an error when reference, blob, or reclamation metadata access fails.
    ///
    /// # Panics
    /// Panics if blob storage violates its canonical SHA-256 digest invariant.
    pub fn reclaim_pass(
        &self,
        meta: &MetaStore,
        blobs: &BlobStorage,
        clock: &Clock,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
        fence: u64,
        batch: NonZeroUsize,
    ) -> Result<AvailabilityTaskReport, AvailabilityTaskError> {
        if fence == 0 {
            return Ok(AvailabilityTaskReport::default());
        }
        let now = clock();
        let referenced = match self.references.referenced() {
            Ok(referenced) => referenced,
            Err(reason) => return Err(AvailabilityTaskError::new("reclamation_references", reason)),
        };
        let required = match meta.current_serial() {
            Ok(required) => required,
            Err(error) => return Err(task_error("reclamation_frontier_read", error)),
        };
        let mut report = AvailabilityTaskReport::default();

        let mut stored = Vec::new();
        let scan = blobs.blocking().visit(|entry| {
            if let Some(digest) = entry.digest {
                stored.push(digest);
            }
            Ok::<(), Infallible>(())
        });
        if let Err(error) = scan {
            return Err(task_error("reclamation_scan", error));
        }
        for digest in stored.into_iter().take(batch.get()) {
            if cancelled() {
                return Ok(report);
            }
            report.processed += 1;
            if referenced.contains(digest.as_str()) {
                continue;
            }
            let artifact = ArtifactDigest::from_sha256(digest.as_str())
                .expect("blob storage only yields canonical SHA-256 digests");
            match select_reclamation_candidate(meta, &artifact, false, required, fence, now) {
                Ok(SelectOutcome::Selected(_)) => report.changed += 1,
                Ok(_) => {}
                Err(error) => return Err(task_error("reclamation_select", error)),
            }
        }

        let Some(observed) = self.frontiers.observe() else {
            return Ok(report);
        };
        let tombstones = match meta.reclamation_tombstones() {
            Ok(tombstones) => tombstones,
            Err(error) => return Err(task_error("reclamation_read", error)),
        };
        for tombstone in tombstones {
            if cancelled() {
                return Ok(report);
            }
            if !matches!(tombstone.state, ReclamationState::Pending) {
                continue;
            }
            let referenced_now = referenced.contains(tombstone.digest.sha256());
            match mark_reclamation_ready(meta, &tombstone.digest, referenced_now, observed, fence, now) {
                Ok(ReadyOutcome::Ready(_)) => report.changed += 1,
                Ok(_) => {}
                Err(error) => return Err(task_error("reclamation_mark", error)),
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
            .reclaim_pass(&self.meta, &self.blobs, &self.clock, cancelled, fence, batch)
    }
}

fn task_error(code: &'static str, error: impl std::fmt::Display) -> AvailabilityTaskError {
    AvailabilityTaskError::new(code, error.to_string())
}

#[cfg(test)]
#[path = "../tests/unit/reclamation_runtime_tests.rs"]
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
