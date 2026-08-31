//! Reads verified remote placements after a local miss. Trusted placement metadata bounds staging, ranges
//! stream into an unpublished stage, and whole-blob verification guards publication.
//!
//! The placement lifecycle alone creates local placement records because it owns their authority fence.

use std::collections::HashMap;
use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::Arc;
use std::time::Duration;

use crate::{
    BlobTransport, CircuitBreaker, CircuitConfig, CircuitPermit, DEFAULT_CIRCUIT, DEFAULT_RANGED_PULL_BUDGET,
    DEFAULT_RECONNECT_POLICY, FetchPlan, PlacementDescriptor, RangedPullBudget, ReconnectPolicy, Retry,
    StagedPullError, TransportError, plan_blob_fetch, pull_blob_staged, route_blob_placements,
};
use peryx_ha::{
    BlobAvailability, BlobAvailabilityError, BlobAvailabilityFailure, BlobPlacementRecord, BlobPlacementState,
    DataCenterId,
};
use peryx_identity::ArtifactDigest;
use peryx_storage::blob::{BlobError, BlobMetadata, BlobStorage, ChunkedDigest, Digest};
use peryx_storage::meta::{MetaError, MetaStore};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadThroughOutcome {
    Served(BlobMetadata),
    Unavailable,
}

#[derive(Debug, thiserror::Error)]
pub enum ReadThroughError {
    #[error("read-through could not read blob placements: {0}")]
    Meta(#[source] MetaError),
    #[error("read-through could not stage the fetched blob: {0}")]
    Blob(#[source] BlobError),
}

/// Returns Unix time in seconds for circuit-breaker cooldowns.
pub type MonotonicClock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// Shared across requests so the concurrency limit does not reset for each fetch.
pub type DcTransport = Arc<dyn BlobTransport + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadThroughLimits {
    /// Maximum concurrent streams per data center.
    pub concurrency: NonZeroUsize,
    /// Maximum bytes materialized by one ranged fetch.
    pub per_fetch_bytes: NonZeroU64,
    /// Bytes requested per range when no catalog is stored. A stored catalog owns the boundaries instead,
    /// because its chunk digests only verify its own spans.
    pub chunk_bytes: NonZeroUsize,
    pub max_fanout: NonZeroUsize,
    pub circuit: CircuitConfig,
    pub policy: ReconnectPolicy,
}

pub const DEFAULT_READ_THROUGH_LIMITS: ReadThroughLimits = ReadThroughLimits {
    concurrency: NonZeroUsize::new(8).expect("8 is non-zero"),
    per_fetch_bytes: NonZeroU64::new(64 * 1024 * 1024).expect("64 MiB is non-zero"),
    chunk_bytes: NonZeroUsize::new(8 * 1024 * 1024).expect("8 MiB is non-zero"),
    max_fanout: NonZeroUsize::new(4).expect("4 is non-zero"),
    circuit: DEFAULT_CIRCUIT,
    policy: DEFAULT_RECONNECT_POLICY,
};

impl Default for ReadThroughLimits {
    fn default() -> Self {
        DEFAULT_READ_THROUGH_LIMITS
    }
}

struct Source {
    data_center: String,
    transport: DcTransport,
    size: u64,
}

struct AdmittedSource<'a> {
    source: &'a Source,
    permit: Option<CircuitPermit>,
}

/// Shares per-data-center transports and circuit state across requests.
pub struct RemotePlacementReader {
    meta: MetaStore,
    blobs: BlobStorage,
    local_dc: DataCenterId,
    delegates: HashMap<String, DcTransport>,
    circuit: CircuitBreaker,
    budget: RangedPullBudget,
    max_fanout: NonZeroUsize,
    policy: ReconnectPolicy,
}

impl RemotePlacementReader {
    /// `delegates` maps remote data centers to their transports; omit `local_dc`.
    #[must_use]
    pub fn new(
        meta: MetaStore,
        blobs: BlobStorage,
        local_dc: DataCenterId,
        delegates: HashMap<String, DcTransport>,
        limits: ReadThroughLimits,
        clock: MonotonicClock,
    ) -> Self {
        Self {
            meta,
            blobs,
            local_dc,
            delegates,
            circuit: CircuitBreaker::new(
                limits.circuit,
                Arc::new(move || Duration::from_secs((clock)().max(0).unsigned_abs())),
            ),
            budget: RangedPullBudget {
                range_bytes: limits.chunk_bytes,
                ..DEFAULT_RANGED_PULL_BUDGET
            },
            max_fanout: limits.max_fanout,
            policy: limits.policy,
        }
    }

    /// Returns [`ReadThroughOutcome::Unavailable`] without committing when no verified source succeeds.
    /// [`ReadThroughOutcome::Served`] means the local store committed bytes matching `digest`.
    ///
    /// # Errors
    /// Returns [`ReadThroughError`] for local metadata or storage failures. Remote failures produce
    /// [`ReadThroughOutcome::Unavailable`].
    ///
    /// # Panics
    /// Panics if `digest` violates the [`Digest`] SHA-256 representation invariant.
    pub async fn read_through(&self, digest: &Digest) -> Result<ReadThroughOutcome, ReadThroughError> {
        let artifact = ArtifactDigest::from_sha256(digest.as_str()).expect("a blob digest is a valid sha256");
        let routing = route_blob_placements(
            self.meta.blob_placements(&artifact).map_err(ReadThroughError::Meta)?,
            &self.local_dc,
        );
        let Some((sources, total_length)) = self.select(&routing.verified_remote) else {
            return Ok(ReadThroughOutcome::Unavailable);
        };
        let catalog = self.meta.blob_chunk_digest(&artifact).map_err(ReadThroughError::Meta)?;
        self.fetch(&artifact, digest, &sources, total_length, catalog.as_ref())
            .await
    }

    /// Selects at most one reachable source per data center in canonical placement order. The transfer
    /// length comes from the selected verified placement record.
    fn select(&self, verified_remote: &[BlobPlacementRecord]) -> Option<(Vec<Source>, usize)> {
        let sizes: HashMap<&str, u64> = verified_remote
            .iter()
            .filter_map(|record| verified_size(record).map(|size| (record.key.data_center.as_str(), size)))
            .collect();
        let descriptors: Vec<PlacementDescriptor> = verified_remote.iter().map(PlacementDescriptor::from).collect();
        let FetchPlan::Sources(ordered) = plan_blob_fetch(&descriptors, self.local_dc.as_str()) else {
            return None;
        };
        let mut sources: Vec<Source> = Vec::new();
        for descriptor in ordered {
            if sources.len() == self.max_fanout.get() {
                break;
            }
            if sources
                .iter()
                .any(|source| source.data_center == descriptor.data_center)
            {
                continue;
            }
            let Some(transport) = self.delegates.get(&descriptor.data_center) else {
                continue;
            };
            let size = sizes.get(descriptor.data_center.as_str()).copied().unwrap_or_default();
            sources.push(Source {
                data_center: descriptor.data_center,
                transport: Arc::clone(transport),
                size,
            });
        }
        if sources.is_empty() {
            return None;
        }
        let total_length = usize::try_from(sources[0].size).unwrap_or(usize::MAX);
        Some((sources, total_length))
    }

    /// Ranges stream into an unpublished stage under the pull budget, so a first fetch of a multi-gigabyte
    /// artifact holds the budget rather than the artifact. Nothing becomes visible before the whole digest
    /// verifies, and a pull that had no catalog records the digests the pass derived so later reads can
    /// verify each range as it arrives.
    ///
    /// Transport exhaustion retries from a fresh stage; a terminal range failure or a digest mismatch
    /// returns [`ReadThroughOutcome::Unavailable`] with nothing published.
    async fn fetch(
        &self,
        artifact: &ArtifactDigest,
        digest: &Digest,
        sources: &[Source],
        total_length: usize,
        catalog: Option<&ChunkedDigest>,
    ) -> Result<ReadThroughOutcome, ReadThroughError> {
        let mut attempt = 1u32;
        loop {
            let mut admitted = self.admit(sources);
            if admitted.is_empty() {
                return Ok(ReadThroughOutcome::Unavailable);
            }
            let transports: Vec<&(dyn BlobTransport + Send + Sync)> =
                admitted.iter().map(|source| source.source.transport.as_ref()).collect();
            let pull = pull_blob_staged(&self.blobs, &transports, digest, total_length, catalog, self.budget).await;
            match pull {
                Ok(staged) => {
                    admitted[0].success();
                    if let Some(chunks) = &staged.chunks {
                        catalog_chunks(&self.meta, artifact, digest, chunks);
                    }
                    return Ok(ReadThroughOutcome::Served(BlobMetadata {
                        bytes: staged.receipt.size,
                        modified: None,
                    }));
                }
                Err(StagedPullError::Stage(error)) => return Err(ReadThroughError::Blob(error)),
                Err(StagedPullError::DigestMismatch { .. }) => return Ok(ReadThroughOutcome::Unavailable),
                Err(StagedPullError::RangeUnavailable(unavailable)) => {
                    let failures = unavailable.transport_failures();
                    record_failures(&mut admitted, &failures);
                    if failures.is_empty() {
                        return Ok(ReadThroughOutcome::Unavailable);
                    }
                    match self.policy.on_error(representative(&failures), attempt) {
                        Retry::After(delay) => {
                            tokio::time::sleep(delay).await;
                            attempt += 1;
                        }
                        Retry::GiveUp { .. } => return Ok(ReadThroughOutcome::Unavailable),
                    }
                }
            }
        }
    }

    fn admit<'a>(&self, sources: &'a [Source]) -> Vec<AdmittedSource<'a>> {
        sources
            .iter()
            .filter_map(|source| {
                self.circuit.admit(&source.data_center).map(|permit| AdmittedSource {
                    source,
                    permit: Some(permit),
                })
            })
            .collect()
    }
}

impl AdmittedSource<'_> {
    fn success(&mut self) {
        self.permit
            .take()
            .expect("an admitted source has one unresolved permit")
            .success();
    }

    fn failure(&mut self) {
        self.permit
            .take()
            .expect("an admitted source has one unresolved permit")
            .failure();
    }
}

fn record_failures(admitted: &mut [AdmittedSource<'_>], failures: &[(usize, TransportError)]) {
    for (index, _) in failures {
        admitted[*index].failure();
    }
}

#[async_trait::async_trait]
impl BlobAvailability for RemotePlacementReader {
    async fn ensure_local(
        &self,
        digest: &Digest,
    ) -> Result<Option<peryx_storage::blob::BlobMetadata>, BlobAvailabilityError> {
        match self.read_through(digest).await {
            Ok(ReadThroughOutcome::Served(metadata)) => Ok(Some(metadata)),
            Ok(ReadThroughOutcome::Unavailable) => Ok(None),
            Err(ReadThroughError::Meta(error)) => {
                Err(BlobAvailabilityError::new(BlobAvailabilityFailure::Placement, error))
            }
            Err(ReadThroughError::Blob(error)) => {
                Err(BlobAvailabilityError::new(BlobAvailabilityFailure::Storage, error))
            }
        }
    }
}

const fn verified_size(record: &BlobPlacementRecord) -> Option<u64> {
    match record.state {
        BlobPlacementState::Verified { size } => Some(size),
        _ => None,
    }
}

/// Chunk metadata is a cache; its write failure must not fail a committed blob.
fn catalog_chunks(meta: &MetaStore, artifact: &ArtifactDigest, digest: &Digest, chunked: &ChunkedDigest) {
    if let Err(error) = meta.put_blob_chunk_digest(artifact, chunked) {
        tracing::warn!(digest = digest.as_str(), %error, "could not catalog blob chunk digests");
    }
}

/// Prefer a retryable failure when any source can recover; preserve the first terminal reason when none
/// can recover.
fn representative(failures: &[(usize, TransportError)]) -> &TransportError {
    failures
        .iter()
        .map(|(_, error)| error)
        .find(|error| error.is_retryable())
        .unwrap_or(&failures[0].1)
}

#[cfg(test)]
#[path = "../tests/unit/read_through/tests.rs"]
mod tests;
