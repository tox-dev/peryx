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
//! # Deferred frontier source
//!
//! The [`ObservedFrontier`] a candidate's readiness gates on - how far each replica and backup has
//! durably applied - has no live runtime source wired here yet. The replica dimension can now ride the
//! merged liveness beacon: a writer's `LivenessTracker::applied_frontier` reports each replica's last
//! confirmed serial and the group's frontier is their minimum. Threading it is a follow-up, because the
//! selector is composed in the binary from configuration alone while the tracker lives inside the
//! replication runtime; a `ServingState` seam the composition root registers, mirroring
//! `set_blob_reclaimer`, feeds the observer without coupling the two construction sites. The backup
//! dimension stays deferred until a backup-applied serial exists, exactly as the hosted-write ack defers
//! its cross-datacenter receipt transport. So [`DeferredFrontiers`] reports the closed `{0, 0}` frontier:
//! selection and tombstoning run live, and readiness stays conservative until the beacon feeds the
//! observer. A test injects a [`ReclamationFrontiers`] stub to exercise every readiness arm.

use std::collections::BTreeSet;
use std::convert::Infallible;
use std::sync::Arc;

use async_trait::async_trait;

use peryx_driver::jobs::{JobFailure, JobReport, ReclamationParameters};
use peryx_driver::serving::EcosystemDriver;
use peryx_driver::state::ServingState;
use peryx_ha::{AvailabilityTaskError, AvailabilityTaskReport};
use peryx_identity::ArtifactDigest;
use peryx_storage::meta::{MetaStore, ObservedFrontier, ReadyOutcome, ReclamationState, SelectOutcome};

use crate::config::Config;

/// The set of blob digests a reclamation pass must keep: every digest metadata or trash still names.
///
/// Injected so a test drives selection against a chosen reference set without the global driver registry.
trait ReferenceInventory: Send + Sync {
    /// The bare-hex sha256 of every digest still referenced, so a candidate in this set is spared.
    fn referenced(&self, meta: &MetaStore) -> Result<BTreeSet<String>, String>;
}

/// The observed replication frontiers a reclamation pass gates candidate readiness on.
///
/// Injected so a test drives readiness against chosen frontiers; the production source is deferred (see
/// the module documentation).
trait ReclamationFrontiers: Send + Sync {
    /// The lowest metadata serial every live replica and configured backup has durably applied.
    fn observe(&self) -> ObservedFrontier;
}

/// The production reference inventory: every ecosystem's referenced blobs, unioned with trashed digests,
/// because a trashed artifact can be restored and so still pins its bytes.
struct DriverReferences {
    /// The configured index names whose trash the reclaimer consults.
    index_names: Vec<String>,
}

impl ReferenceInventory for DriverReferences {
    fn referenced(&self, meta: &MetaStore) -> Result<BTreeSet<String>, String> {
        collect_references(
            crate::app::referenced_blob_digests(meta),
            crate::server::drivers().present(),
            meta,
            &self.index_names,
        )
    }
}

/// Union `base` with every installed ecosystem's trashed digests, because a trashed artifact can be
/// restored and so still pins its bytes. Kept free of the global driver registry and the reference
/// collector so a test drives its scan-failure and digest-folding arms against chosen drivers.
fn collect_references<'a>(
    base: anyhow::Result<BTreeSet<String>>,
    drivers: impl Iterator<Item = &'a Arc<dyn EcosystemDriver>>,
    meta: &MetaStore,
    index_names: &[String],
) -> Result<BTreeSet<String>, String> {
    let mut referenced = base.map_err(|reason| reason.to_string())?;
    for driver in drivers {
        let Some(trash) = driver.capabilities().trash else {
            continue;
        };
        let records = trash
            .trash_records(meta, index_names)
            .map_err(|reason| format!("scan {} trash: {reason}", driver.ecosystem().as_str()))?;
        for record in records {
            if let Some(digest) = record.digest {
                referenced.insert(digest.strip_prefix("sha256:").unwrap_or(&digest).to_owned());
            }
        }
    }
    Ok(referenced)
}

/// The production frontier observer while the live per-member applied serial is deferred: it reports the
/// closed `{0, 0}` frontier, so a candidate at a nonzero required frontier stays pending until the beacon
/// source lands, and nothing is marked ready on incomplete evidence.
struct DeferredFrontiers;

impl ReclamationFrontiers for DeferredFrontiers {
    fn observe(&self) -> ObservedFrontier {
        ObservedFrontier { replica: 0, backup: 0 }
    }
}

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
    /// Build the selector for a node from its configuration.
    ///
    /// Returns `None` when the node reclaims nothing: no data-center membership, or this node's writer
    /// identity absent from the roster. The live frontier source is deferred, so the selector observes the
    /// closed frontier until the liveness beacon feeds it.
    ///
    /// # Errors
    /// Never returns an error today; the fallible signature matches the sibling
    /// [`CrossDcBlobCopier`](super::CrossDcBlobCopier) registration so the binary threads them the same
    /// way.
    pub fn from_config(config: &Config) -> anyhow::Result<Option<Self>> {
        let (Some(membership), Some(identity)) = (config.dc_membership.as_ref(), config.writer_identity.as_deref())
        else {
            return Ok(None);
        };
        if !membership.members.iter().any(|member| member.node == identity) {
            return Ok(None);
        }
        let index_names = config.indexes.iter().map(|index| index.name.clone()).collect();
        Ok(Some(Self {
            references: Arc::new(DriverReferences { index_names }),
            frontiers: Arc::new(DeferredFrontiers),
        }))
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
        let observed = self.frontiers.observe();
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
#[path = "reclamation_tests.rs"]
mod tests;
