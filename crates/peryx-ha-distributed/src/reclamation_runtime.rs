use std::collections::BTreeSet;
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
use peryx_storage::meta::{MetaStore, ReclamationPhase};

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
        let required = match meta.current_serial() {
            Ok(required) => required,
            Err(error) => return Err(task_error("reclamation_frontier_read", error)),
        };
        let mut report = AvailabilityTaskReport::default();

        let Some(selected_against) = self.prove_references(meta)? else {
            return Ok(report);
        };
        let selection = read_cursor(meta, ReclamationPhase::Selection)?;
        let stored = match blobs.blocking().digest_page(selection.as_deref(), batch) {
            Ok(page) => page,
            Err(error) => return Err(task_error("reclamation_scan", error)),
        };
        for digest in stored.digests {
            if cancelled() {
                return Ok(report);
            }
            report.processed += 1;
            if selected_against.digests.contains(digest.as_str()) {
                continue;
            }
            let artifact = ArtifactDigest::from_sha256(digest.as_str())
                .expect("blob storage only yields canonical SHA-256 digests");
            match select_reclamation_candidate(meta, &artifact, false, selected_against.revision, required, fence, now)
            {
                Ok(Some(SelectOutcome::Selected(_))) => report.changed += 1,
                Ok(Some(_)) => {}
                Ok(None) => return Ok(report),
                Err(error) => return Err(task_error("reclamation_select", error)),
            }
        }
        advance_cursor(meta, ReclamationPhase::Selection, stored.next_cursor.as_deref())?;

        let Some(observed) = self.frontiers.observe() else {
            return Ok(report);
        };
        let Some(marked_against) = self.prove_references(meta)? else {
            return Ok(report);
        };
        let finalize = read_cursor(meta, ReclamationPhase::Finalize)?;
        let tombstones = match meta.scan_reclamation_tombstones(finalize.as_deref(), batch) {
            Ok(page) => page,
            Err(error) => return Err(task_error("reclamation_read", error)),
        };
        for tombstone in tombstones.records {
            if cancelled() {
                return Ok(report);
            }
            if !matches!(tombstone.state, ReclamationState::Pending) {
                continue;
            }
            let referenced_now = marked_against.digests.contains(tombstone.digest.sha256());
            match mark_reclamation_ready(
                meta,
                &tombstone.digest,
                referenced_now,
                marked_against.revision,
                observed,
                fence,
                now,
            ) {
                Ok(Some(ReadyOutcome::Ready(_))) => report.changed += 1,
                Ok(Some(_)) => {}
                Ok(None) => return Ok(report),
                Err(error) => return Err(task_error("reclamation_mark", error)),
            }
        }
        advance_cursor(meta, ReclamationPhase::Finalize, tombstones.next_cursor.as_deref())?;
        Ok(report)
    }

    /// Reads the reference revision on both sides of the inventory scan and keeps the result only when
    /// it did not move, so a reference committed part way through the scan retires the inventory
    /// instead of leaving a digest that one component already reported as unreferenced.
    ///
    /// `None` ends the pass; the cursors stay put and the next pass proves a fresh inventory.
    fn prove_references(&self, meta: &MetaStore) -> Result<Option<ProvedReferences>, AvailabilityTaskError> {
        let revision = reference_revision(meta)?;
        let digests = match self.references.referenced() {
            Ok(digests) => digests,
            Err(reason) => return Err(AvailabilityTaskError::new("reclamation_references", reason)),
        };
        Ok((reference_revision(meta)? == revision).then_some(ProvedReferences { revision, digests }))
    }
}

/// The referenced digests together with the revision that proves nothing changed while they were read.
struct ProvedReferences {
    revision: u64,
    digests: BTreeSet<String>,
}

fn reference_revision(meta: &MetaStore) -> Result<u64, AvailabilityTaskError> {
    meta.reference_revision()
        .map_err(|error| task_error("reclamation_reference_revision", error))
}

fn read_cursor(meta: &MetaStore, phase: ReclamationPhase) -> Result<Option<String>, AvailabilityTaskError> {
    meta.reclamation_cursor(phase)
        .map_err(|error| task_error("reclamation_cursor_read", error))
}

/// A page without a successor ends the scan, and clearing the cursor wraps the next pass back to the
/// first row. Writing after the page is processed may repeat a page after a crash, never skip one.
fn advance_cursor(
    meta: &MetaStore,
    phase: ReclamationPhase,
    cursor: Option<&str>,
) -> Result<(), AvailabilityTaskError> {
    meta.set_reclamation_cursor(phase, cursor)
        .map_err(|error| task_error("reclamation_cursor_write", error))
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
