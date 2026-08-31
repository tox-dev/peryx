//! Placement records are digest-keyed and node-wide, so the pass uses the ownership group's cluster term.
//! Term `0` disables placement writes. A newer term may repeat a copy; stale placements remain fenced.
use std::collections::HashMap;
use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt as _;
use futures_util::stream::FuturesUnordered;

use crate::copy_planning::{CrossDcCopy, copy_backlog_entry, plan_cross_dc_copy};
use crate::placement_policy::apply_blob_placement;
use crate::{BlobTransport, CopyError, HttpBlobTransport, TransferLimits, TransportError, copy_blob_to_target};
use peryx_core::Clock;
use peryx_ha::{
    AvailabilityTaskError, AvailabilityTaskReport, BackendId, BlobPlacementFailure, BlobPlacementKey,
    BlobPlacementOutcome, BlobPlacementTransition, DataCenterId,
};
use peryx_storage::blob::{BlobStore, Digest};
use peryx_storage::meta::MetaStore;

const SOURCE_FETCH_TIMEOUT: Duration = Duration::from_secs(30);
/// A pass stops planning at the first page boundary that reaches `limit`, so it holds one page of
/// planned copies at a time and attempts at most `limit + batch - 1` of them before it yields.
const PASS_PACING: PassPacing = PassPacing {
    batch: NonZeroUsize::new(256).unwrap(),
    limit: NonZeroUsize::new(4096).unwrap(),
};

#[derive(Clone, Copy)]
struct PassPacing {
    batch: NonZeroUsize,
    limit: NonZeroUsize,
}

enum ScanPosition {
    Start,
    After(String),
    End,
}

struct BacklogScan {
    position: ScanPosition,
    planned: usize,
    error: Option<AvailabilityTaskError>,
}

impl BacklogScan {
    fn resuming(cursor: Option<String>) -> Self {
        Self {
            position: cursor.map_or(ScanPosition::Start, ScanPosition::After),
            planned: 0,
            error: None,
        }
    }

    /// Where the next pass starts; `None` restarts at the first placement.
    fn resume(&self) -> Option<&str> {
        match &self.position {
            ScanPosition::After(cursor) => Some(cursor),
            ScanPosition::Start | ScanPosition::End => None,
        }
    }
}

trait SourceTransports: Send + Sync {
    fn transport(&self, source_dc: &str) -> Option<&(dyn BlobTransport + Send + Sync)>;
}

struct RosterTransports {
    transports: HashMap<String, HttpBlobTransport>,
}

impl RosterTransports {
    fn new(roster: HashMap<String, String>, token: &str) -> Result<Self, crate::HttpBlobError> {
        let transports = roster
            .into_iter()
            .map(|(data_center, base)| {
                HttpBlobTransport::new(&base, token.to_owned(), TransferLimits::default(), SOURCE_FETCH_TIMEOUT)
                    .map(|transport| (data_center, transport))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { transports })
    }
}

impl SourceTransports for RosterTransports {
    fn transport(&self, source_dc: &str) -> Option<&(dyn BlobTransport + Send + Sync)> {
        self.transports
            .get(source_dc)
            .map(|transport| transport as &(dyn BlobTransport + Send + Sync))
    }
}

pub struct CrossDcBlobCopier {
    local_dc: DataCenterId,
    backend: BackendId,
    store: BlobStore,
    sources: Arc<dyn SourceTransports>,
}

impl CrossDcBlobCopier {
    /// # Errors
    /// Returns an error when a roster address or the replication token cannot build an HTTP transport.
    pub fn http(
        local_dc: DataCenterId,
        roster: HashMap<String, String>,
        token: &str,
        store: BlobStore,
        backend: BackendId,
    ) -> Result<Option<Self>, crate::HttpBlobError> {
        if roster.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self {
            local_dc,
            backend,
            store,
            sources: Arc::new(RosterTransports::new(roster, token)?),
        }))
    }

    /// Returns the next page of planned copies, or `None` once the pass stops planning because it
    /// was cancelled, reached its per-pass cap, exhausted the index, or failed to read it.
    fn next_page(
        &self,
        meta: &MetaStore,
        scan: &mut BacklogScan,
        fence: NonZeroU64,
        pacing: PassPacing,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
    ) -> Option<Vec<CrossDcCopy>> {
        if scan.planned >= pacing.limit.get() || cancelled() {
            return None;
        }
        let cursor = match &scan.position {
            ScanPosition::Start => None,
            ScanPosition::After(cursor) => Some(cursor.clone()),
            ScanPosition::End => return None,
        };
        let page = match meta.scan_blob_placement_groups(cursor.as_deref(), pacing.batch) {
            Ok(page) => page,
            Err(error) => {
                scan.error = Some(task_error("copy_backlog_scan", error));
                return None;
            }
        };
        let planned: Vec<CrossDcCopy> = page
            .groups
            .iter()
            .filter_map(|records| {
                copy_backlog_entry(records, &self.local_dc, fence)
                    .map(|entry| plan_cross_dc_copy(&entry, &self.local_dc, &self.backend, fence))
            })
            .collect();
        scan.planned += planned.len();
        scan.position = page.next_cursor.map_or(ScanPosition::End, ScanPosition::After);
        Some(planned)
    }

    async fn copy_one(&self, meta: &MetaStore, clock: &Clock, copy: CrossDcCopy) -> bool {
        let source_dc = copy.source.data_center.as_str();
        let Some(transport) = self.sources.transport(source_dc) else {
            tracing::warn!(source_dc, "cross-datacenter copy has no reachable source peer");
            return false;
        };
        let Some(BlobPlacementOutcome::Applied(staged)) = record(
            meta,
            &copy.target,
            &BlobPlacementTransition::Stage,
            copy.fence.get(),
            clock,
        ) else {
            return false;
        };
        let digest = Digest::from_hex(copy.target.digest.sha256()).expect("artifact digests are validated SHA-256");
        let outcome = copy_blob_to_target(transport, &self.store, &digest).await;
        let transition = match &outcome {
            Ok(()) => BlobPlacementTransition::Verify {
                attempt: staged.transfer_attempt,
                observed: copy.target.digest.clone(),
                size: copy.size,
            },
            Err(error) => BlobPlacementTransition::Fail {
                attempt: staged.transfer_attempt,
                class: failure_class(error),
            },
        };
        let recorded = record(meta, &copy.target, &transition, copy.fence.get(), clock).is_some();
        outcome.is_ok() && recorded
    }
}

impl CrossDcBlobCopier {
    /// # Errors
    /// Returns an error when the placement backlog read fails, or when the pass cannot read or record
    /// the cursor the next pass resumes from.
    pub async fn copy_pass(
        &self,
        meta: &MetaStore,
        clock: &Clock,
        fence: u64,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
        concurrency: NonZeroUsize,
    ) -> Result<AvailabilityTaskReport, AvailabilityTaskError> {
        self.paced_copy_pass(meta, clock, fence, cancelled, concurrency, PASS_PACING)
            .await
    }

    /// Feeds each planned page straight into the bounded-concurrency stage, so the first copy starts
    /// one page into the scan rather than after it, and the report counts attempts rather than plans.
    async fn paced_copy_pass(
        &self,
        meta: &MetaStore,
        clock: &Clock,
        fence: u64,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
        concurrency: NonZeroUsize,
        pacing: PassPacing,
    ) -> Result<AvailabilityTaskReport, AvailabilityTaskError> {
        let Some(fence) = NonZeroU64::new(fence) else {
            return Ok(AvailabilityTaskReport::default());
        };
        let resumed = meta
            .blob_copy_cursor(self.local_dc.as_str())
            .map_err(|error| task_error("copy_cursor_read", error))?;
        let mut scan = BacklogScan::resuming(resumed.clone());
        let mut planning = true;
        let mut page = Vec::new().into_iter();
        let mut in_flight = FuturesUnordered::new();
        let mut report = AvailabilityTaskReport::default();
        loop {
            while planning && in_flight.len() < concurrency.get() {
                if let Some(copy) = page.next() {
                    in_flight.push(self.copy_one(meta, clock, copy));
                } else {
                    match self.next_page(meta, &mut scan, fence, pacing, cancelled) {
                        Some(next) => page = next.into_iter(),
                        None => planning = false,
                    }
                }
            }
            let Some(recorded) = in_flight.next().await else {
                break;
            };
            report.processed += 1;
            report.changed += u64::from(recorded);
        }
        if let Some(error) = scan.error {
            return Err(error);
        }
        if scan.resume() != resumed.as_deref() {
            meta.set_blob_copy_cursor(self.local_dc.as_str(), scan.resume())
                .map_err(|error| task_error("copy_cursor_write", error))?;
        }
        Ok(report)
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
) -> Option<BlobPlacementOutcome> {
    match apply_blob_placement(meta, key, transition, fence, (clock)()) {
        Ok(outcome) => Some(outcome),
        Err(error) => {
            tracing::warn!(%error, ?transition, "cross-datacenter copy could not record a placement");
            None
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
