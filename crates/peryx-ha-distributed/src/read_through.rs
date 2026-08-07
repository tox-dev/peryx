//! Serving-path read-through of a public download from a verified remote placement.
//!
//! When a public download misses the local content store but a peer data center holds a verified
//! placement of the same digest, this fetches the bytes from that peer, on demand, and stages them
//! locally so the request - and every later one - serves from disk. It is the serving counterpart to the
//! background cross-DC copier: the copier fills the local store ahead of demand from the backlog, this
//! fills it in response to a miss.
//!
//! # What it composes
//! The pieces already exist; this only wires them into one capability an ecosystem miss path can call:
//! [`route_blob_placements`](peryx_storage::meta::MetaStore::route_blob_placements) classifies the
//! digest's placements, [`plan_blob_fetch`] orders the verified remote ones, a per-data-center
//! [`BlobTransport`] reaches each peer, [`pull_ranged`] draws the blob in bounded ranges and falls
//! through to the next source on a loss, a [`CircuitBreaker`] skips a peer that keeps failing, a
//! [`ReconnectPolicy`] bounds the retries, and the blob store stages the bytes to disk and verifies the
//! whole-blob digest before it publishes them.
//!
//! # Trusted size, not a peer's word
//! The total length a fetch reassembles against is read from the local verified placement record's own
//! `size`, replicated metadata this node already trusts - never a peer's advertisement - so a lying peer
//! cannot inflate the reassembly buffer.
//!
//! # Verify before store
//! [`pull_ranged`] digest-verifies the reassembled whole against the requested digest, and the blob
//! store's commit verifies it again at the content address; a corrupt or failed source therefore leaves
//! no local content and no served response, only a fall-through to the next source or, when none can
//! answer, an [`Unavailable`](ReadThroughOutcome::Unavailable) that the caller resolves through its own
//! upstream path.
//!
//! Advertising the fetched bytes as a local placement is deliberately out of scope: that record is
//! fence-coupled and owned by the placement lifecycle, so this only populates the content store.
//!
//! # Incremental chunk-verified staging
//! When the [`blob_chunk_digest`](peryx_storage::meta::MetaStore::blob_chunk_digest) catalog holds the
//! digest's [`ChunkedDigest`] - recorded by a node that whole-verified the same content - a read-through
//! streams the blob chunk by chunk instead of reassembling the whole in memory: it draws each chunk range,
//! verifies it against its own recorded digest through [`pull_chunk_verified`], and stages it to disk
//! before drawing the next. One chunk is held in memory at a time and each is written before the next is
//! fetched, so a very large blob no longer forces the whole into a reassembly buffer. The commit still
//! whole-verifies the digest as the safety net. A digest with no catalogued chunks falls back to whole-blob
//! staging and records its [`ChunkedDigest`] from that verified pull, so the next fetch of the same content
//! can stream.

use std::collections::HashMap;
use std::num::{NonZeroU64, NonZeroUsize};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::{
    BlobTransport, ChunkUnavailable, CircuitBreaker, CircuitConfig, DEFAULT_CIRCUIT, DEFAULT_RECONNECT_POLICY,
    FetchPlan, PlacementDescriptor, PullError, ReconnectPolicy, Retry, TransportError, chunk_ranges, plan_blob_fetch,
    pull_chunk_verified, pull_ranged,
};
use bytes::Bytes;
use peryx_ha::{ReadThroughError, ReadThroughOutcome, RemoteBlobReader};
use peryx_identity::ArtifactDigest;
use peryx_storage::blob::{BlobErrorKind, BlobStorage, CHUNK_BYTES, ChunkedDigest, Digest};
use peryx_storage::meta::{BlobPlacementRecord, BlobPlacementState, DataCenterId, MetaStore};

/// A source of the current unix time the circuit breaker measures cooldowns against.
///
/// Injectable so a test drives the breaker with explicit timestamps. It is the process clock the serving
/// state already holds, so the caller passes it straight through rather than adapting it.
pub type MonotonicClock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// A per-data-center blob transport, shared and long-lived so its concurrency bound holds across
/// requests rather than resetting per fetch.
pub type DcTransport = Arc<dyn BlobTransport + Send + Sync>;

/// The tunable bounds a read-through runs under.
///
/// `concurrency` and `per_fetch_bytes` shape the per-data-center transports the caller builds - the
/// in-flight-stream ceiling and the byte cap each fetch streams under - while `chunk_bytes`,
/// `max_fanout`, `circuit`, and `policy` shape the fetch itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadThroughLimits {
    /// The most concurrent streams one data center's transport runs, the memory bound: a saturated
    /// read-through holds at most this many reassembling blobs per peer in memory at once.
    pub concurrency: NonZeroUsize,
    /// The byte cap each ranged fetch streams under, so a fast or hostile peer cannot force an unbounded
    /// read.
    pub per_fetch_bytes: NonZeroU64,
    /// The span of one ranged request, so a large blob is drawn in bounded pieces.
    pub chunk_bytes: NonZeroUsize,
    /// The most verified remote sources one fetch tries before it gives up.
    pub max_fanout: NonZeroUsize,
    /// When a source trips open and how long it stays open.
    pub circuit: CircuitConfig,
    /// How the fetch backs off and when it stops retrying.
    pub policy: ReconnectPolicy,
}

/// A conservative default for every read-through bound.
///
/// Eight concurrent streams per peer, a 64 MiB per-fetch cap, 8 MiB ranges, up to four sources, tripping
/// a source after three losses for thirty seconds, and the default reconnect schedule.
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

/// What one chunk-verified streaming pass produced.
enum StreamOutcome {
    /// Every chunk was verified, staged, and the whole blob committed under its digest.
    Committed,
    /// A chunk could not be drawn from any open source; the pass carries the per-source failures so the
    /// caller can trip circuits and decide whether a retry could recover.
    ChunkUnavailable(ChunkUnavailable),
    /// Every chunk verified against the catalog but the committed whole failed its digest - a colluding
    /// catalog and source the safety net rejected. Nothing was left in the store.
    WholeMismatch,
}

/// One verified remote placement selected as a fetch source: the data center whose circuit gates it, the
/// transport that reaches it, and the trusted byte length it advertises through replicated metadata.
struct Source {
    data_center: String,
    transport: DcTransport,
    size: u64,
}

/// Serves a public download from a verified remote placement when the local content store misses.
///
/// Holds the per-data-center transports long-lived so their concurrency bound is a real ceiling across
/// requests, and one circuit breaker shared across every fetch so a peer that keeps failing is skipped
/// until it cools down.
pub struct RemotePlacementReader {
    local_dc: DataCenterId,
    delegates: HashMap<String, DcTransport>,
    circuit: Mutex<CircuitBreaker>,
    chunk_bytes: NonZeroUsize,
    max_fanout: NonZeroUsize,
    policy: ReconnectPolicy,
    clock: MonotonicClock,
}

impl RemotePlacementReader {
    /// Build a reader for `local_dc` over the per-data-center `delegates`, bounded by `limits` and
    /// timed by `clock`.
    ///
    /// A delegate is keyed by the data center it reaches; the local data center holds no delegate,
    /// since a local miss is exactly why a read-through runs. `clock` supplies the logical time the
    /// circuit breaker measures cooldowns against.
    #[must_use]
    pub fn new(
        local_dc: DataCenterId,
        delegates: HashMap<String, DcTransport>,
        limits: ReadThroughLimits,
        clock: MonotonicClock,
    ) -> Self {
        Self {
            local_dc,
            delegates,
            circuit: Mutex::new(CircuitBreaker::new(limits.circuit)),
            chunk_bytes: limits.chunk_bytes,
            max_fanout: limits.max_fanout,
            policy: limits.policy,
            clock,
        }
    }

    /// Fetch `digest` from a verified remote placement and stage it into `blobs`, returning whether the
    /// blob is now present locally.
    ///
    /// A non-sha256 digest, a digest with no verified remote placement, sources all open or exhausted,
    /// or a source whose bytes do not verify all yield [`Unavailable`](ReadThroughOutcome::Unavailable)
    /// with nothing committed. A [`Served`](ReadThroughOutcome::Served) means the digest-verified bytes
    /// are committed to the content store.
    ///
    /// # Errors
    /// Returns [`ReadThroughError`] when the placement ledger cannot be read or the verified bytes cannot
    /// be staged locally - a local fault the caller resolves through its upstream path, not a remote one.
    ///
    /// # Panics
    /// Never in practice: a blob [`Digest`] is always a 64-character lowercase-hex sha256, exactly what
    /// [`ArtifactDigest::from_sha256`] accepts.
    pub async fn read_through(
        &self,
        meta: &MetaStore,
        blobs: &BlobStorage,
        digest: &Digest,
    ) -> Result<ReadThroughOutcome, ReadThroughError> {
        let artifact = ArtifactDigest::from_sha256(digest.as_str()).expect("a blob digest is a valid sha256");
        let routing = meta
            .route_blob_placements(&artifact, &self.local_dc)
            .map_err(ReadThroughError::Meta)?;
        let Some((sources, total_length)) = self.select(&routing.verified_remote) else {
            return Ok(ReadThroughOutcome::Unavailable);
        };
        match meta.blob_chunk_digest(&artifact).map_err(ReadThroughError::Meta)? {
            Some(chunked) => {
                self.fetch_streaming(blobs, digest, &sources, total_length, &chunked)
                    .await
            }
            None => self.fetch(meta, &artifact, blobs, digest, &sources, total_length).await,
        }
    }

    /// Order the verified remote placements into fetch sources and read the trusted total length.
    ///
    /// [`plan_blob_fetch`] gives the canonical order (highest generation first, then a stable tie-break);
    /// this keeps the sources whose data center has a reachable transport, drops a data center already
    /// seen so one peer is tried once, and caps the list at the configured fan-out. The total length is
    /// the selected source's own verified `size` - replicated metadata this node trusts. `None` when no
    /// verified remote placement has a reachable transport.
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

    /// Draw `digest` across the open sources in order and stage the verified whole, retrying transient
    /// exhaustion under the reconnect policy.
    ///
    /// This is the fallback taken when no chunk digests are catalogued for `digest`: it reassembles the
    /// whole blob, verifies it, commits it, and records its [`ChunkedDigest`] so the next fetch of the same
    /// content streams chunk by chunk instead.
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
                    self.commit(blobs, digest, bytes).await?;
                    catalog_chunks(meta, artifact, digest, &chunked);
                    return Ok(ReadThroughOutcome::Served);
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

    /// Stream `digest` chunk by chunk from the open sources, staging each verified chunk into the content
    /// store as it arrives, and retry transient exhaustion under the reconnect policy.
    ///
    /// Each pass draws every chunk of `chunked` in order, verifying it against its own recorded digest
    /// before it is staged, so a bad chunk falls through to another source without the whole blob ever
    /// entering memory. A pass that exhausts a chunk over reachable transports backs off and retries from
    /// a fresh stage; one that exhausts on non-transport failures - or whose whole-blob commit fails its
    /// safety-net verify against a colluding catalog and source - is [`Unavailable`](ReadThroughOutcome).
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
                StreamOutcome::Committed => {
                    self.record_success(&open[0].data_center);
                    return Ok(ReadThroughOutcome::Served);
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

    /// Draw every chunk of `chunked` from the open `sources` into a pending blob and commit the whole.
    ///
    /// One chunk is held in memory at a time and each is written to disk before the next is drawn, so the
    /// footprint is bounded regardless of blob size. The commit whole-verifies the digest as the safety
    /// net over the trusted per-chunk digests; a mismatch there means the catalog and a source agreed on
    /// bytes that are not the requested content, which leaves no local blob and yields
    /// [`WholeMismatch`](StreamOutcome::WholeMismatch).
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
            Ok(_) => Ok(StreamOutcome::Committed),
            Err(error) if error.kind() == BlobErrorKind::DigestMismatch => Ok(StreamOutcome::WholeMismatch),
            Err(error) => Err(ReadThroughError::Blob(error)),
        }
    }

    /// Stage `bytes` to disk and publish them only if they verify against `digest`, so a mismatch leaves
    /// no local content.
    async fn commit(&self, blobs: &BlobStorage, digest: &Digest, bytes: Bytes) -> Result<(), ReadThroughError> {
        let mut pending = blobs.begin().await.map_err(ReadThroughError::Blob)?;
        pending.write_chunk(bytes).await.map_err(ReadThroughError::Blob)?;
        pending.commit(digest).await.map(|_| ()).map_err(ReadThroughError::Blob)
    }

    /// The circuit breaker's logical now, the process unix time floored at zero as a duration since the
    /// epoch, so a pre-epoch clock never underflows the cooldown math.
    fn now(&self) -> Duration {
        Duration::from_secs(u64::try_from((self.clock)().max(0)).unwrap_or(0))
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
impl RemoteBlobReader for RemotePlacementReader {
    async fn read_through(
        &self,
        meta: &MetaStore,
        blobs: &BlobStorage,
        digest: &Digest,
    ) -> Result<ReadThroughOutcome, ReadThroughError> {
        Self::read_through(self, meta, blobs, digest).await
    }
}

/// The byte size a verified placement records, or `None` for any other state. A router hands this only
/// verified placements, but the accessor is total over the lifecycle so it never assumes that.
const fn verified_size(record: &BlobPlacementRecord) -> Option<u64> {
    match record.state {
        BlobPlacementState::Verified { size } => Some(size),
        _ => None,
    }
}

/// Record `chunked` for `digest` in the catalog so a later read-through streams it, tolerating a write
/// failure: the blob is already committed and serveable, so a metadata cache miss must not fail the
/// request that produced it.
fn catalog_chunks(meta: &MetaStore, artifact: &ArtifactDigest, digest: &Digest, chunked: &ChunkedDigest) {
    if let Err(error) = meta.put_blob_chunk_digest(artifact, chunked) {
        tracing::warn!(digest = digest.as_str(), %error, "could not catalog blob chunk digests");
    }
}

/// The failure that decides the retry: a retryable one so the policy backs off if any source could
/// recover, otherwise the first, whose terminal reason makes the policy give up at once.
fn representative(failures: &[(usize, TransportError)]) -> &TransportError {
    failures
        .iter()
        .map(|(_, error)| error)
        .find(|error| error.is_retryable())
        .unwrap_or(&failures[0].1)
}

#[cfg(test)]
mod tests;
