//! The background cross-data-center filesystem blob copier.
//!
//! Local-filesystem HA needs each data center to hold its own verified copy of every blob a peer serves.
//! This is the orchestrator behind the scheduled [`DcCopy`](peryx_driver::jobs::ScheduledJob::DcCopy)
//! job: it reads the copy backlog the local data center owes from the placement ledger, pulls each owed
//! digest from a verified peer over the replication blob transport, and records the resulting local
//! placement.
//!
//! # Fencing
//!
//! The copy is fenced by the ownership group's cluster-level monotonic epoch, [`ClusterStatus::term`],
//! not by a per-repository authority epoch. The copy backlog is digest-keyed and node-wide - it names no
//! repository - so a per-repository epoch is undefined here, and a node-wide job resolves one to the
//! closed `0` sentinel that fences all placement writes shut (the trap this design avoids). The copy is a
//! cluster-level replication activity, so the cluster term is its natural fence. Over-fencing by an
//! advanced cluster term only forces a harmless re-copy under the newer term; it can never let a stale
//! copy win, so the choice is HA-safe. A process running no ownership group reads term `0` and copies
//! nothing.
//!
//! [`ClusterStatus::term`]: peryx_driver::state::ClusterStatus::term

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt as _;

use crate::{BlobTransport, CopyError, HttpBlobTransport, TransferLimits, TransportError, copy_blob_to_target};
use peryx_driver::jobs::{DcCopyParameters, JobFailure, JobReport};
use peryx_driver::state::{Clock, ServingState};
use peryx_ha::{AvailabilityTaskError, AvailabilityTaskReport};
use peryx_storage::blob::{BlobStore, Digest};
use peryx_storage::meta::{
    BackendId, BackendLocation, BlobPlacementFailure, BlobPlacementKey, BlobPlacementTransition, CopyBacklogEntry,
    CopyPlan, CrossDcCopy, DataCenterId, MetaStore, plan_cross_dc_copy,
};

/// Distinct digests one backlog scan reads before the pass resumes past its cursor. The pass loops until
/// the ledger is drained, so this only bounds one iteration's read, not the copy it performs.
const SCAN_BATCH: usize = 256;
/// How long one peer blob fetch may run before it is a retryable source loss.
const SOURCE_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// A blob transport reaching a verified source in a named data center, resolved per copy.
trait SourceTransports: Send + Sync {
    /// The transport reaching a verified peer in `source_dc`, or `None` when the data center is not in the
    /// roster or its address does not build a usable client.
    fn transport(&self, source_dc: &str) -> Option<Box<dyn BlobTransport + Send + Sync>>;
}

/// The production source resolver: a bearer-authenticated HTTP transport per rostered peer data center.
struct RosterTransports {
    /// Peer data center to the base URL a copy fetches its blobs from.
    roster: HashMap<String, String>,
    /// The shared replication bearer token every peer accepts.
    token: String,
    limits: TransferLimits,
}

impl SourceTransports for RosterTransports {
    fn transport(&self, source_dc: &str) -> Option<Box<dyn BlobTransport + Send + Sync>> {
        let base = self.roster.get(source_dc)?;
        let transport = HttpBlobTransport::new(base, self.token.clone(), self.limits, SOURCE_FETCH_TIMEOUT).ok()?;
        Some(Box::new(transport))
    }
}

/// The background cross-data-center filesystem blob copier registered on the [`ServingState`].
pub struct CrossDcBlobCopier {
    /// The data center this node copies bytes into.
    local_dc: DataCenterId,
    /// The blob backend the recorded placements name, matching the local store.
    backend: BackendId,
    /// The local filesystem store the copy publishes verified bytes to.
    store: BlobStore,
    sources: Arc<dyn SourceTransports>,
}

struct BoundCrossDcBlobCopier {
    copier: CrossDcBlobCopier,
    state: Arc<ServingState>,
}

impl CrossDcBlobCopier {
    pub fn bind(self, state: Arc<ServingState>) -> Arc<dyn peryx_ha::CrossDcCopier> {
        Arc::new(BoundCrossDcBlobCopier { copier: self, state })
    }
    /// Build the copier for a filesystem node from its resolved configuration and runtime store.
    ///
    /// Returns `None` when the node copies nothing: no roster, no node identity, no replication token,
    /// this node absent from the roster, or a single-data-center group with no peer to pull from.
    ///
    /// The node names its own roster entry through `node_identity`, so the copier resolves its own
    /// datacenter from that; `writer_identity` is the shared writer every node claims and is the same
    /// across the group, so it names one datacenter for all and cannot place this node. It falls back to
    /// `writer_identity` only for a single-writer group that configures no distinct node identity.
    ///
    /// # Errors
    /// Returns an error when the replication token cannot be read or the local data center is not a valid
    /// placement component.
    #[must_use]
    pub fn http(
        local_dc: DataCenterId,
        roster: HashMap<String, String>,
        token: String,
        store: BlobStore,
        backend: BackendId,
    ) -> Option<Self> {
        if roster.is_empty() {
            return None;
        }
        Some(Self {
            local_dc,
            backend,
            store,
            sources: Arc::new(RosterTransports {
                roster,
                token,
                limits: TransferLimits::default(),
            }),
        })
    }

    fn plan_one(&self, entry: &CopyBacklogEntry, fence: u64) -> Option<CrossDcCopy> {
        let location = BackendLocation::for_digest(&entry.digest);
        match plan_cross_dc_copy(entry, &self.local_dc, &self.backend, &location, fence) {
            CopyPlan::Copy(copy) => Some(*copy),
            CopyPlan::Fenced | CopyPlan::NoSource => None,
        }
    }

    fn collect_backlog(
        &self,
        meta: &MetaStore,
        fence: u64,
        batch: usize,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<Vec<CrossDcCopy>, JobFailure> {
        let mut planned = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            if cancelled() {
                break;
            }
            let page = meta
                .scan_cross_dc_copy_backlog(&self.local_dc, cursor.as_deref(), batch)
                .map_err(|error| JobFailure::new("copy_backlog_scan", error.to_string()))?;
            for entry in &page.entries {
                if let Some(copy) = self.plan_one(entry, fence) {
                    planned.push(copy);
                }
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(planned)
    }

    async fn copy_one(&self, meta: &MetaStore, clock: &Clock, copy: CrossDcCopy, fence: u64) -> bool {
        let source_dc = copy.source.data_center.as_str();
        let Some(transport) = self.sources.transport(source_dc) else {
            tracing::warn!(source_dc, "cross-datacenter copy has no reachable source peer");
            return false;
        };
        if !record(meta, &copy.target, &BlobPlacementTransition::Stage, fence, clock) {
            return false;
        }
        let digest = Digest::from(&copy.target.digest);
        let outcome = copy_blob_to_target(transport.as_ref(), &self.store, &digest).await;
        let transition = match &outcome {
            Ok(()) => BlobPlacementTransition::Verify {
                observed: copy.target.digest.clone(),
                size: copy.size,
            },
            Err(error) => BlobPlacementTransition::Fail {
                class: failure_class(error),
            },
        };
        let recorded = record(meta, &copy.target, &transition, fence, clock);
        outcome.is_ok() && recorded
    }
}

impl CrossDcBlobCopier {
    pub(crate) async fn copy_pass(
        &self,
        state: &ServingState,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
        params: DcCopyParameters,
    ) -> Result<JobReport, JobFailure> {
        let fence = state
            .ownership_authority()
            .map_or(0, |authority| authority.cluster_status().term);
        if fence == 0 {
            return Ok(JobReport::default());
        }
        let planned = self.collect_backlog(&state.meta, fence, SCAN_BATCH, cancelled)?;
        let processed = planned.len() as u64;
        let meta = &state.meta;
        let clock = &state.clock;
        let changed = futures_util::stream::iter(planned)
            .map(|copy| self.copy_one(meta, clock, copy, fence))
            .buffer_unordered(params.concurrency.get())
            .filter(|recorded| std::future::ready(*recorded))
            .count()
            .await as u64;
        Ok(JobReport { processed, changed })
    }
}

#[async_trait]
impl peryx_ha::CrossDcCopier for BoundCrossDcBlobCopier {
    async fn copy_pass(
        &self,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
        concurrency: std::num::NonZeroUsize,
    ) -> Result<AvailabilityTaskReport, AvailabilityTaskError> {
        self.copier
            .copy_pass(&self.state, cancelled, DcCopyParameters { concurrency })
            .await
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

/// Apply one placement transition under `fence`, reporting whether it committed. A rejected transition
/// is logged and counted as not recorded rather than aborting the pass.
fn record(
    meta: &MetaStore,
    key: &BlobPlacementKey,
    transition: &BlobPlacementTransition,
    fence: u64,
    clock: &Clock,
) -> bool {
    match meta.apply_blob_placement(key, transition, fence, (clock)()) {
        Ok(_) => true,
        Err(error) => {
            tracing::warn!(%error, ?transition, "cross-datacenter copy could not record a placement");
            false
        }
    }
}

/// Classify a copy failure into the placement evidence it records.
///
/// A hash mismatch marks the source's bytes permanently bad, a fetch loss marks the source unreachable,
/// and a publish loss marks the target backend at fault.
const fn failure_class(error: &CopyError) -> BlobPlacementFailure {
    match error {
        CopyError::Fetch(TransportError::DigestMismatch { .. }) => BlobPlacementFailure::DigestMismatch,
        CopyError::Fetch(_) => BlobPlacementFailure::SourceUnavailable,
        CopyError::Publish(_) => BlobPlacementFailure::BackendRejected,
    }
}

#[cfg(test)]
#[path = "copy_runtime_tests.rs"]
mod tests;
