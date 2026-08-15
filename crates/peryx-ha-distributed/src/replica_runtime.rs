use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use peryx_ha::{ReplicaPage, ReplicaViewApplier};
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::MetaStore;

use crate::{
    AvailabilityMetrics, BlobPlaneReport, BlobSources, CapacityLimited, ChangePage, HttpBlobTransport,
    HttpPeerTransport, PROTOCOL_VERSION, PeerSet, ReconnectPolicy, Replica, ReplicaMonitor, Retry, SyncError,
    SyncOutcome, TransportError, advance_blob_frontier, pull_outstanding, pull_round,
};

pub const REPLICA_BLOB_FETCH_CONCURRENCY: std::num::NonZeroUsize =
    std::num::NonZeroUsize::new(8).expect("8 is non-zero");

pub struct ReplicaLoop {
    views: Arc<dyn ReplicaViewApplier>,
    metadata: PeerSet<HttpPeerTransport>,
    clock_origin: Instant,
    policy: ReconnectPolicy,
    meta: MetaStore,
    blobs: BlobStorage,
    page_size: std::num::NonZeroUsize,
    poll_interval: Duration,
    monitor: Arc<ReplicaMonitor>,
    metrics: Arc<AvailabilityMetrics>,
    transport: CapacityLimited<HttpBlobTransport>,
    local_dc: String,
    delegates: HashMap<String, CapacityLimited<HttpBlobTransport>>,
}

pub struct ReplicaLoopParts {
    pub views: Arc<dyn ReplicaViewApplier>,
    pub metadata: PeerSet<HttpPeerTransport>,
    pub policy: ReconnectPolicy,
    pub meta: MetaStore,
    pub blobs: BlobStorage,
    pub page_size: std::num::NonZeroUsize,
    pub poll_interval: Duration,
    pub monitor: Arc<ReplicaMonitor>,
    pub metrics: Arc<AvailabilityMetrics>,
    pub transport: CapacityLimited<HttpBlobTransport>,
    pub local_dc: String,
    pub delegates: HashMap<String, CapacityLimited<HttpBlobTransport>>,
}

pub fn schedule_delay(
    result: &Result<bool, TransportError>,
    attempt: &mut u32,
    policy: &ReconnectPolicy,
    poll_interval: Duration,
) -> Duration {
    match result {
        Ok(caught_up) => {
            *attempt = 0;
            if *caught_up { poll_interval } else { Duration::ZERO }
        }
        Err(error) => {
            *attempt += 1;
            match policy.on_error(error, *attempt) {
                Retry::After(delay) => delay,
                Retry::GiveUp { .. } => poll_interval,
            }
        }
    }
}

impl ReplicaLoop {
    #[must_use]
    pub fn new(parts: ReplicaLoopParts) -> Self {
        Self {
            views: parts.views,
            metadata: parts.metadata,
            clock_origin: Instant::now(),
            policy: parts.policy,
            meta: parts.meta,
            blobs: parts.blobs,
            page_size: parts.page_size,
            poll_interval: parts.poll_interval,
            monitor: parts.monitor,
            metrics: parts.metrics,
            transport: parts.transport,
            local_dc: parts.local_dc,
            delegates: parts.delegates,
        }
    }

    pub async fn run(mut self) {
        let mut attempt: u32 = 0;
        loop {
            let result = self.cycle().await;
            let delay = schedule_delay(&result, &mut attempt, &self.policy, self.poll_interval);
            tokio::time::sleep(delay).await;
        }
    }

    /// Advances metadata before blobs. The readable frontier is the slower view, so reads cannot name
    /// absent bytes. The loop retries blob failures without blocking metadata progress.
    ///
    /// Validation and commit failures retry at the poll interval; metadata transport loss triggers backoff.
    ///
    /// # Errors
    /// Returns the metadata transport failure for this cycle.
    pub async fn cycle(&mut self) -> Result<bool, TransportError> {
        let started = Instant::now();
        let now = self.clock_origin.elapsed();
        let state = match Replica::new(&self.meta, self.page_size).state() {
            Ok(state) => state,
            Err(error) => return Ok(self.record_metadata_error(&error, started.elapsed())),
        };
        let after = state.as_ref().map_or(0, |state| state.serial);
        // A fresh replica pins its first commit to the source that answers this round.
        let committed = state.map(|state| state.source);

        let views = &self.views;
        let meta = &self.meta;
        let page_size = self.page_size;
        let apply = move |page: ChangePage| -> Result<u64, SyncError> {
            let (outcome, changed, _referenced) = Replica::new(meta, page_size).apply_page(page)?;
            views.apply(
                ReplicaPage {
                    changes: outcome.changes,
                    serial: outcome.serial,
                    primary_serial: outcome.primary_serial,
                },
                &changed,
            );
            views.publish_applied_frontier(outcome.serial);
            Ok(outcome.serial)
        };
        let round = match pull_round(&mut self.metadata, now, after, committed.as_deref(), apply).await {
            Ok(round) => round,
            Err(error) => return Ok(self.record_metadata_error(&error, started.elapsed())),
        };
        let elapsed = started.elapsed();
        if let Some(actual) = round.incompatible {
            let error = SyncError::UnsupportedVersion {
                actual,
                expected: PROTOCOL_VERSION,
            };
            return Ok(self.record_metadata_error(&error, elapsed));
        }
        if !round.answered {
            let loss = TransportError::Disconnected;
            self.record_metadata_error(&SyncError::primary(loss.clone()), elapsed);
            return Err(loss);
        }

        let outcome = SyncOutcome {
            changes: round.applied,
            serial: round.serial,
            primary_serial: round.head.max(round.serial),
        };
        match self.pull_blobs().await {
            Ok(report) => self.monitor.record_blobs(report),
            Err(error) => {
                self.monitor.record_error(&error);
                self.metrics.record_error(&error, elapsed);
                tracing::error!(%error, "replica blob plane failed");
            }
        }
        self.monitor.record(outcome);
        let readable = self.views.readable_frontier();
        self.monitor.record_readable(readable);
        self.metrics.record_cycle(outcome, elapsed);
        Ok(round.caught_up)
    }

    /// Returns `true` so validation and commit failures retry at the poll interval instead of backing off.
    fn record_metadata_error(&self, error: &SyncError, elapsed: Duration) -> bool {
        self.monitor.record_error(error);
        self.metrics.record_error(error, elapsed);
        tracing::error!(%error, "replica metadata synchronization failed");
        true
    }

    async fn pull_blobs(&self) -> Result<BlobPlaneReport, SyncError> {
        // Read-through serves blobs placed on reachable peers; the replica pulls local or unreachable
        // placements in full. An unresolved data center makes the replica pull every blob.
        let reachable: BTreeSet<String> = self.delegates.keys().cloned().collect();
        let sources = BlobSources {
            simple: &self.transport,
            delegates: &self.delegates,
            local_dc: &self.local_dc,
        };
        let report = pull_outstanding(
            &sources,
            &self.meta,
            &self.blobs,
            self.page_size,
            REPLICA_BLOB_FETCH_CONCURRENCY,
        )
        .await?;
        advance_blob_frontier(&self.meta, &self.blobs, self.page_size, &self.local_dc, &reachable).await?;
        Ok(report)
    }
}

#[cfg(test)]
#[path = "../tests/unit/replica_runtime_tests.rs"]
mod tests;
