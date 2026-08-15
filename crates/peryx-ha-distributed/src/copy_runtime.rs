//! Placement records are digest-keyed and node-wide, so the pass uses the ownership group's cluster term.
//! Term `0` disables placement writes. A newer term may repeat a copy; stale placements remain fenced.
use std::collections::HashMap;
use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt as _;

use crate::copy_planning::{CrossDcCopy, copy_backlog_entry, plan_cross_dc_copy};
use crate::placement_policy::apply_blob_placement;
use crate::{BlobTransport, CopyError, HttpBlobTransport, TransferLimits, TransportError, copy_blob_to_target};
use peryx_core::Clock;
use peryx_ha::{
    AvailabilityTaskError, AvailabilityTaskReport, BackendId, BlobPlacementFailure, BlobPlacementKey,
    BlobPlacementTransition, DataCenterId,
};
use peryx_storage::blob::{BlobStore, Digest};
use peryx_storage::meta::MetaStore;

const SCAN_BATCH: NonZeroUsize = NonZeroUsize::new(256).unwrap();
const SOURCE_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

trait SourceTransports: Send + Sync {
    fn transport(&self, source_dc: &str) -> Option<Box<dyn BlobTransport + Send + Sync>>;
}

struct RosterTransports {
    roster: HashMap<String, String>,
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

pub struct CrossDcBlobCopier {
    local_dc: DataCenterId,
    backend: BackendId,
    store: BlobStore,
    sources: Arc<dyn SourceTransports>,
}

impl CrossDcBlobCopier {
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

    fn collect_backlog(
        &self,
        meta: &MetaStore,
        fence: NonZeroU64,
        batch: NonZeroUsize,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<Vec<CrossDcCopy>, AvailabilityTaskError> {
        let mut planned = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            if cancelled() {
                break;
            }
            let page = meta
                .scan_blob_placement_groups(cursor.as_deref(), batch)
                .map_err(|error| task_error("copy_backlog_scan", error))?;
            planned.extend(page.groups.iter().filter_map(|records| {
                copy_backlog_entry(records, &self.local_dc)
                    .map(|entry| plan_cross_dc_copy(&entry, &self.local_dc, &self.backend, fence))
            }));
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(planned)
    }

    async fn copy_one(&self, meta: &MetaStore, clock: &Clock, copy: CrossDcCopy) -> bool {
        let source_dc = copy.source.data_center.as_str();
        let Some(transport) = self.sources.transport(source_dc) else {
            tracing::warn!(source_dc, "cross-datacenter copy has no reachable source peer");
            return false;
        };
        if !record(
            meta,
            &copy.target,
            &BlobPlacementTransition::Stage,
            copy.fence.get(),
            clock,
        ) {
            return false;
        }
        let digest = Digest::from_hex(copy.target.digest.sha256()).expect("artifact digests are validated SHA-256");
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
        let recorded = record(meta, &copy.target, &transition, copy.fence.get(), clock);
        outcome.is_ok() && recorded
    }
}

impl CrossDcBlobCopier {
    /// # Errors
    /// Returns an error when the placement backlog read fails.
    pub async fn copy_pass(
        &self,
        meta: &MetaStore,
        clock: &Clock,
        fence: u64,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
        concurrency: NonZeroUsize,
    ) -> Result<AvailabilityTaskReport, AvailabilityTaskError> {
        let Some(fence) = NonZeroU64::new(fence) else {
            return Ok(AvailabilityTaskReport::default());
        };
        let planned = self.collect_backlog(meta, fence, SCAN_BATCH, cancelled)?;
        let processed = planned.len() as u64;
        let changed = futures_util::stream::iter(planned)
            .map(|copy| self.copy_one(meta, clock, copy))
            .buffer_unordered(concurrency.get())
            .filter(|recorded| std::future::ready(*recorded))
            .count()
            .await as u64;
        Ok(AvailabilityTaskReport { processed, changed })
    }
}

fn task_error(code: &'static str, error: impl std::fmt::Display) -> AvailabilityTaskError {
    AvailabilityTaskError::new(code, error.to_string())
}

fn record(
    meta: &MetaStore,
    key: &BlobPlacementKey,
    transition: &BlobPlacementTransition,
    fence: u64,
    clock: &Clock,
) -> bool {
    match apply_blob_placement(meta, key, transition, fence, (clock)()) {
        Ok(_) => true,
        Err(error) => {
            tracing::warn!(%error, ?transition, "cross-datacenter copy could not record a placement");
            false
        }
    }
}

/// Records source failures as digest mismatch or unavailability. Records publish failures as target
/// backend rejection.
const fn failure_class(error: &CopyError) -> BlobPlacementFailure {
    match error {
        CopyError::Fetch(TransportError::DigestMismatch { .. }) => BlobPlacementFailure::DigestMismatch,
        CopyError::Fetch(_) => BlobPlacementFailure::SourceUnavailable,
        CopyError::Publish(_) => BlobPlacementFailure::BackendRejected,
    }
}

#[cfg(test)]
#[path = "../tests/unit/copy_runtime_tests.rs"]
mod tests;
