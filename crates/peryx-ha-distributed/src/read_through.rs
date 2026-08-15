//! Reads verified remote placements after a local miss. Trusted placement metadata bounds staging, and
//! whole-blob verification guards publication.
//!
//! The placement lifecycle alone creates local placement records because it owns their authority fence.

use std::collections::HashMap;
use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::{
    BlobTransport, ChunkUnavailable, CircuitBreaker, CircuitConfig, DEFAULT_CIRCUIT, DEFAULT_RECONNECT_POLICY,
    FetchPlan, PlacementDescriptor, PullError, ReconnectPolicy, Retry, TransportError, chunk_ranges, plan_blob_fetch,
    pull_chunk_verified, pull_ranged, route_blob_placements,
};
use bytes::Bytes;
use peryx_ha::{
    BlobAvailability, BlobAvailabilityError, BlobAvailabilityFailure, BlobPlacementRecord, BlobPlacementState,
    DataCenterId,
};
use peryx_identity::ArtifactDigest;
use peryx_storage::blob::{BlobError, BlobErrorKind, BlobMetadata, BlobStorage, CHUNK_BYTES, ChunkedDigest, Digest};
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

enum StreamOutcome {
    Committed(BlobMetadata),
    ChunkUnavailable(ChunkUnavailable),
    /// Whole-digest verification failed after all chunks matched the catalog; no local blob remains.
    WholeMismatch,
}

struct Source {
    data_center: String,
    transport: DcTransport,
    size: u64,
}

/// Shares per-data-center transports and circuit state across requests.
pub struct RemotePlacementReader {
    meta: MetaStore,
    blobs: BlobStorage,
    local_dc: DataCenterId,
    delegates: HashMap<String, DcTransport>,
    circuit: Mutex<CircuitBreaker>,
    chunk_bytes: NonZeroUsize,
    max_fanout: NonZeroUsize,
    policy: ReconnectPolicy,
    clock: MonotonicClock,
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
            circuit: Mutex::new(CircuitBreaker::new(limits.circuit)),
            chunk_bytes: limits.chunk_bytes,
            max_fanout: limits.max_fanout,
            policy: limits.policy,
            clock,
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
        match self.meta.blob_chunk_digest(&artifact).map_err(ReadThroughError::Meta)? {
            Some(chunked) => {
                self.fetch_streaming(&self.blobs, digest, &sources, total_length, &chunked)
                    .await
            }
            None => {
                self.fetch(&self.meta, &artifact, &self.blobs, digest, &sources, total_length)
                    .await
            }
        }
    }

    /// Selects at most one reachable source per data center in canonical placement order. The reassembly
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

    /// Without cataloged chunk digests, verifies and commits the whole blob before recording chunk
    /// digests for later streaming reads.
    async fn fetch(
        &self,
        meta: &MetaStore,
        artifact: &ArtifactDigest,
        blobs: &BlobStorage,
        digest: &Digest,
        sources: &[Source],
        total_length: usize,
    ) -> Result<ReadThroughOutcome, ReadThroughError> {
        let ranges = chunk_ranges(total_length, self.chunk_bytes.get());
        let mut attempt = 1u32;
        loop {
            let now = self.now();
            let open: Vec<&Source> = sources
                .iter()
                .filter(|source| self.available(&source.data_center, now))
                .collect();
            if open.is_empty() {
                return Ok(ReadThroughOutcome::Unavailable);
            }
            let transports: Vec<&(dyn BlobTransport + Send + Sync)> =
                open.iter().map(|source| source.transport.as_ref()).collect();
            match pull_ranged(&transports, digest, &ranges, total_length, digest).await {
                Ok(bytes) => {
                    self.record_success(&open[0].data_center);
                    let chunked = ChunkedDigest::of(&bytes, CHUNK_BYTES);
                    let metadata = self.commit(blobs, digest, bytes).await?;
                    catalog_chunks(meta, artifact, digest, &chunked);
                    return Ok(ReadThroughOutcome::Served(metadata));
                }
                Err(PullError::Exhausted { failures, .. }) => {
                    self.record_failures(&open, &failures, now);
                    match self.policy.on_error(representative(&failures), attempt) {
                        Retry::After(delay) => {
                            tokio::time::sleep(delay).await;
                            attempt += 1;
                        }
                        Retry::GiveUp { .. } => return Ok(ReadThroughOutcome::Unavailable),
                    }
                }
                Err(PullError::Piece(_) | PullError::Reassembly(_)) => return Ok(ReadThroughOutcome::Unavailable),
            }
        }
    }

    /// Verifies each chunk before staging it. Transport exhaustion retries from a new stage; terminal
    /// chunk failure or whole-digest mismatch returns [`ReadThroughOutcome::Unavailable`].
    async fn fetch_streaming(
        &self,
        blobs: &BlobStorage,
        digest: &Digest,
        sources: &[Source],
        total_length: usize,
        chunked: &ChunkedDigest,
    ) -> Result<ReadThroughOutcome, ReadThroughError> {
        let mut attempt = 1u32;
        loop {
            let now = self.now();
            let open: Vec<&Source> = sources
                .iter()
                .filter(|source| self.available(&source.data_center, now))
                .collect();
            if open.is_empty() {
                return Ok(ReadThroughOutcome::Unavailable);
            }
            match self.stream_chunks(blobs, digest, chunked, total_length, &open).await? {
                StreamOutcome::Committed(metadata) => {
                    self.record_success(&open[0].data_center);
                    return Ok(ReadThroughOutcome::Served(metadata));
                }
                StreamOutcome::WholeMismatch => return Ok(ReadThroughOutcome::Unavailable),
                StreamOutcome::ChunkUnavailable(unavailable) => {
                    let failures = unavailable.transport_failures();
                    self.record_failures(&open, &failures, now);
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

    /// Holds one chunk in memory at a time. Commit verifies the whole digest in case the catalog and a
    /// source agree on bytes for different content.
    async fn stream_chunks(
        &self,
        blobs: &BlobStorage,
        digest: &Digest,
        chunked: &ChunkedDigest,
        total_length: usize,
        open: &[&Source],
    ) -> Result<StreamOutcome, ReadThroughError> {
        let transports: Vec<&(dyn BlobTransport + Send + Sync)> =
            open.iter().map(|source| source.transport.as_ref()).collect();
        let mut write = blobs.begin().await.map_err(ReadThroughError::Blob)?;
        for index in 0..chunked.len() {
            match pull_chunk_verified(&transports, digest, chunked, index, total_length as u64).await {
                Ok(bytes) => write.write_chunk(bytes).await.map_err(ReadThroughError::Blob)?,
                Err(unavailable) => return Ok(StreamOutcome::ChunkUnavailable(unavailable)),
            }
        }
        match write.commit(digest).await {
            Ok(receipt) => Ok(StreamOutcome::Committed(BlobMetadata {
                bytes: receipt.size,
                modified: None,
            })),
            Err(error) if error.kind() == BlobErrorKind::DigestMismatch => Ok(StreamOutcome::WholeMismatch),
            Err(error) => Err(ReadThroughError::Blob(error)),
        }
    }

    /// Whole-digest verification must pass before staged bytes are published.
    async fn commit(
        &self,
        blobs: &BlobStorage,
        digest: &Digest,
        bytes: Bytes,
    ) -> Result<BlobMetadata, ReadThroughError> {
        let mut pending = blobs.begin().await.map_err(ReadThroughError::Blob)?;
        pending.write_chunk(bytes).await.map_err(ReadThroughError::Blob)?;
        pending
            .commit(digest)
            .await
            .map(|receipt| BlobMetadata {
                bytes: receipt.size,
                modified: None,
            })
            .map_err(ReadThroughError::Blob)
    }

    /// Floors pre-epoch clock values at zero to prevent cooldown underflow.
    fn now(&self) -> Duration {
        Duration::from_secs((self.clock)().max(0).unsigned_abs())
    }

    fn available(&self, data_center: &str, now: Duration) -> bool {
        self.circuit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .available(data_center, now)
    }

    fn record_success(&self, data_center: &str) {
        self.circuit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .record_success(data_center);
    }

    fn record_failures(&self, open: &[&Source], failures: &[(usize, TransportError)], now: Duration) {
        let mut breaker = self.circuit.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        for (index, _) in failures {
            breaker.record_failure(&open[*index].data_center, now);
        }
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
