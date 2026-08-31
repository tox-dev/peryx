use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use crate::{
    BlobRequest, BlobTransport, ByteRange, CapacityLimited, CircuitConfig, DEFAULT_CIRCUIT, DEFAULT_RECONNECT_POLICY,
    ReconnectPolicy, TransportError,
};
use async_trait::async_trait;
use bytes::Bytes;
use peryx_ha::{
    BackendId, BackendLocation, BlobAvailability, BlobAvailabilityFailure, BlobPlacementFailure, BlobPlacementKey,
    BlobPlacementRecord, BlobPlacementState, BlobPlacementTransition, DataCenterId,
};
use peryx_identity::ArtifactDigest;
use peryx_storage::blob::{BlobError, BlobStorage, CHUNK_BYTES, ChunkedDigest, Digest};
use peryx_storage::meta::MetaStore;

use super::{
    DEFAULT_READ_THROUGH_LIMITS, DcTransport, MonotonicClock, ReadThroughError, ReadThroughLimits, ReadThroughOutcome,
    RemotePlacementReader, representative, verified_size,
};

#[derive(Clone, Copy)]
enum Corruption {
    None,
    Content,
    Short,
}

struct Peer {
    content: Bytes,
    fail_first: AtomicUsize,
    error: TransportError,
    corruption: Corruption,
}

#[async_trait]
impl BlobTransport for Peer {
    async fn fetch_blob(&self, request: BlobRequest) -> Result<Vec<u8>, TransportError> {
        if self
            .fail_first
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| remaining.checked_sub(1))
            .is_ok()
        {
            return Err(self.error.clone());
        }
        let range = request.range.unwrap_or(ByteRange {
            offset: 0,
            length: self.content.len(),
        });
        let start = range.offset.min(self.content.len());
        let end = start.saturating_add(range.length).min(self.content.len());
        let mut bytes = self.content[start..end].to_vec();
        match self.corruption {
            Corruption::None => {}
            Corruption::Content => bytes.iter_mut().for_each(|byte| *byte ^= 0xFF),
            Corruption::Short => {
                bytes.pop();
            }
        }
        Ok(bytes)
    }
}

fn peer(content: Bytes, fail_first: usize, error: TransportError, corruption: Corruption) -> DcTransport {
    Arc::new(Peer {
        content,
        fail_first: AtomicUsize::new(fail_first),
        error,
        corruption,
    })
}

fn serving(content: &Bytes) -> DcTransport {
    peer(content.clone(), 0, TransportError::Disconnected, Corruption::None)
}

fn delegates<const N: usize>(pairs: [(&str, DcTransport); N]) -> HashMap<String, DcTransport> {
    let mut delegates = HashMap::with_capacity(N);
    for (dc, transport) in pairs {
        delegates.insert(dc.to_owned(), transport);
    }
    delegates
}

fn stores() -> (tempfile::TempDir, MetaStore, BlobStorage) {
    let dir = tempfile::tempdir().unwrap();
    let meta = crate::support::distributed_meta(dir.path().join("peryx.redb"));
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    (dir, meta, blobs)
}

fn key(digest: &Digest, dc: &str, backend: &str, location: &str) -> BlobPlacementKey {
    BlobPlacementKey {
        digest: ArtifactDigest::from_sha256(digest.as_str()).unwrap(),
        backend: BackendId::new(backend).unwrap(),
        data_center: DataCenterId::new(dc).unwrap(),
        location: BackendLocation::new(location).unwrap(),
    }
}

fn seed_verified(meta: &MetaStore, digest: &Digest, dc: &str, backend: &str, location: &str, size: u64) {
    let key = key(digest, dc, backend, location);
    let artifact = ArtifactDigest::from_sha256(digest.as_str()).unwrap();
    crate::apply_blob_placement(meta, &key, &BlobPlacementTransition::Stage, 1, 0).unwrap();
    crate::apply_blob_placement(
        meta,
        &key,
        &BlobPlacementTransition::Verify {
            attempt: 1,
            observed: artifact,
            size,
        },
        1,
        0,
    )
    .unwrap();
}

fn chunked(content: &[u8], size: u64) -> ChunkedDigest {
    ChunkedDigest::of(content, std::num::NonZeroU64::new(size).unwrap())
}

fn seed_chunk_digest(meta: &MetaStore, digest: &Digest, chunked: &ChunkedDigest) {
    let artifact = ArtifactDigest::from_sha256(digest.as_str()).unwrap();
    meta.put_blob_chunk_digest(&artifact, chunked).unwrap();
}

fn stored_chunk_digest(meta: &MetaStore, digest: &Digest) -> Option<ChunkedDigest> {
    let artifact = ArtifactDigest::from_sha256(digest.as_str()).unwrap();
    meta.blob_chunk_digest(&artifact).unwrap()
}

fn frozen_clock(seconds: u64) -> (Arc<AtomicU64>, MonotonicClock) {
    let ticks = Arc::new(AtomicU64::new(seconds));
    let handle = Arc::clone(&ticks);
    let clock: MonotonicClock = Arc::new(move || i64::try_from(handle.load(Ordering::SeqCst)).unwrap_or(i64::MAX));
    (ticks, clock)
}

fn reader(
    meta: &MetaStore,
    blobs: &BlobStorage,
    local_dc: &str,
    delegates: HashMap<String, DcTransport>,
    limits: ReadThroughLimits,
) -> RemotePlacementReader {
    RemotePlacementReader::new(
        meta.clone(),
        blobs.clone(),
        DataCenterId::new(local_dc).unwrap(),
        delegates,
        limits,
        frozen_clock(0).1,
    )
}

fn reader_with_clock(
    meta: &MetaStore,
    blobs: &BlobStorage,
    local_dc: &str,
    delegates: HashMap<String, DcTransport>,
    limits: ReadThroughLimits,
    clock: MonotonicClock,
) -> RemotePlacementReader {
    RemotePlacementReader::new(
        meta.clone(),
        blobs.clone(),
        DataCenterId::new(local_dc).unwrap(),
        delegates,
        limits,
        clock,
    )
}

fn small(bytes: usize) -> ReadThroughLimits {
    ReadThroughLimits {
        chunk_bytes: NonZeroUsize::new(bytes).unwrap(),
        ..DEFAULT_READ_THROUGH_LIMITS
    }
}

#[tokio::test]
async fn test_serves_verified_bytes_from_a_remote_placement() {
    let (_dir, meta, blobs) = stores();
    let content = Bytes::from_static(b"the release archive bytes");
    let digest = Digest::of(&content);
    seed_verified(&meta, &digest, "east", "filesystem", "east/a", content.len() as u64);
    let reader = reader(
        &meta,
        &blobs,
        "home",
        delegates([("east", serving(&content))]),
        DEFAULT_READ_THROUGH_LIMITS,
    );

    let outcome = reader.read_through(&digest).await.unwrap();

    assert!(matches!(outcome, ReadThroughOutcome::Served(_)));
    let stored = blobs.open(&digest, None).await.unwrap();
    assert_eq!(stored.collect(u64::MAX).await.unwrap(), content);
}

#[tokio::test]
async fn test_serves_a_blob_drawn_across_several_ranges() {
    let (_dir, meta, blobs) = stores();
    let content = Bytes::from_static(b"0123456789abcdef");
    let digest = Digest::of(&content);
    seed_verified(&meta, &digest, "east", "filesystem", "east/a", content.len() as u64);
    let reader = reader(
        &meta,
        &blobs,
        "home",
        delegates([("east", serving(&content))]),
        small(4),
    );

    let outcome = reader.read_through(&digest).await.unwrap();

    assert!(matches!(outcome, ReadThroughOutcome::Served(_)));
    assert!(blobs.head(&digest).await.unwrap().is_some());
}

#[tokio::test]
async fn test_no_placement_is_unavailable() {
    let (_dir, meta, blobs) = stores();
    let digest = Digest::of(b"never placed anywhere");
    let reader = reader(&meta, &blobs, "home", delegates([]), DEFAULT_READ_THROUGH_LIMITS);

    let outcome = reader.read_through(&digest).await.unwrap();

    assert_eq!(outcome, ReadThroughOutcome::Unavailable);
    assert!(blobs.head(&digest).await.unwrap().is_none());
}

#[tokio::test]
async fn test_placement_in_a_data_center_without_a_delegate_is_unavailable() {
    let (_dir, meta, blobs) = stores();
    let content = Bytes::from_static(b"held only in an unreachable dc");
    let digest = Digest::of(&content);
    seed_verified(&meta, &digest, "west", "filesystem", "west/a", content.len() as u64);
    let reader = reader(
        &meta,
        &blobs,
        "home",
        delegates([("east", serving(&content))]),
        DEFAULT_READ_THROUGH_LIMITS,
    );

    let outcome = reader.read_through(&digest).await.unwrap();

    assert_eq!(outcome, ReadThroughOutcome::Unavailable);
    assert!(blobs.head(&digest).await.unwrap().is_none());
}

#[tokio::test]
async fn test_corrupt_source_commits_no_local_content() {
    let (_dir, meta, blobs) = stores();
    let content = Bytes::from_static(b"the bytes the catalog names");
    let digest = Digest::of(&content);
    seed_verified(&meta, &digest, "east", "filesystem", "east/a", content.len() as u64);
    let corrupt = peer(content.clone(), 0, TransportError::Disconnected, Corruption::Content);
    let reader = reader(
        &meta,
        &blobs,
        "home",
        delegates([("east", corrupt)]),
        DEFAULT_READ_THROUGH_LIMITS,
    );

    let outcome = reader.read_through(&digest).await.unwrap();

    assert_eq!(outcome, ReadThroughOutcome::Unavailable);
    assert!(blobs.head(&digest).await.unwrap().is_none());
}

#[tokio::test]
async fn test_short_range_source_commits_no_local_content() {
    let (_dir, meta, blobs) = stores();
    let content = Bytes::from_static(b"a source that under-delivers a range");
    let digest = Digest::of(&content);
    seed_verified(&meta, &digest, "east", "filesystem", "east/a", content.len() as u64);
    let short = peer(content.clone(), 0, TransportError::Disconnected, Corruption::Short);
    let reader = reader(
        &meta,
        &blobs,
        "home",
        delegates([("east", short)]),
        DEFAULT_READ_THROUGH_LIMITS,
    );

    let outcome = reader.read_through(&digest).await.unwrap();

    assert_eq!(outcome, ReadThroughOutcome::Unavailable);
    assert!(blobs.head(&digest).await.unwrap().is_none());
}

#[tokio::test]
async fn test_falls_through_to_a_second_source_when_the_first_loses() {
    let (_dir, meta, blobs) = stores();
    let content = Bytes::from_static(b"served by the standby peer");
    let digest = Digest::of(&content);
    seed_verified(&meta, &digest, "east", "filesystem", "east/a", content.len() as u64);
    seed_verified(&meta, &digest, "west", "filesystem", "west/a", content.len() as u64);
    let down = peer(
        content.clone(),
        usize::MAX,
        TransportError::Disconnected,
        Corruption::None,
    );
    let reader = reader(
        &meta,
        &blobs,
        "home",
        delegates([("east", down), ("west", serving(&content))]),
        DEFAULT_READ_THROUGH_LIMITS,
    );

    let outcome = reader.read_through(&digest).await.unwrap();

    assert!(matches!(outcome, ReadThroughOutcome::Served(_)));
    assert!(blobs.head(&digest).await.unwrap().is_some());
}

#[tokio::test]
async fn test_terminal_source_gives_up_without_content() {
    let (_dir, meta, blobs) = stores();
    let content = Bytes::from_static(b"the peer denies holding it");
    let digest = Digest::of(&content);
    seed_verified(&meta, &digest, "east", "filesystem", "east/a", content.len() as u64);
    let missing = peer(
        content.clone(),
        usize::MAX,
        TransportError::BlobNotFound {
            digest: digest.as_str().to_owned(),
        },
        Corruption::None,
    );
    let reader = reader(
        &meta,
        &blobs,
        "home",
        delegates([("east", missing)]),
        DEFAULT_READ_THROUGH_LIMITS,
    );

    let outcome = reader.read_through(&digest).await.unwrap();

    assert_eq!(outcome, ReadThroughOutcome::Unavailable);
    assert!(blobs.head(&digest).await.unwrap().is_none());
}

#[tokio::test(start_paused = true)]
async fn test_retries_a_transient_failure_then_serves() {
    let (_dir, meta, blobs) = stores();
    let content = Bytes::from_static(b"lands on the second attempt");
    let digest = Digest::of(&content);
    seed_verified(&meta, &digest, "east", "filesystem", "east/a", content.len() as u64);
    let flaky = peer(content.clone(), 1, TransportError::Timeout, Corruption::None);
    let limits = ReadThroughLimits {
        circuit: CircuitConfig {
            trip_after: 5,
            ..DEFAULT_CIRCUIT
        },
        ..DEFAULT_READ_THROUGH_LIMITS
    };
    let reader = reader(&meta, &blobs, "home", delegates([("east", flaky)]), limits);

    let outcome = reader.read_through(&digest).await.unwrap();

    assert!(matches!(outcome, ReadThroughOutcome::Served(_)));
    assert!(blobs.head(&digest).await.unwrap().is_some());
}

#[tokio::test]
async fn test_open_circuit_skips_a_source_then_recovers_after_cooldown() {
    let (_dir, meta, blobs) = stores();
    let content = Bytes::from_static(b"one loss trips it, cooldown clears it");
    let digest = Digest::of(&content);
    seed_verified(&meta, &digest, "east", "filesystem", "east/a", content.len() as u64);
    let flaky = peer(content.clone(), 1, TransportError::Timeout, Corruption::None);
    let limits = ReadThroughLimits {
        circuit: CircuitConfig {
            trip_after: 1,
            cooldown: Duration::from_secs(45),
            ..DEFAULT_CIRCUIT
        },
        policy: ReconnectPolicy::new(
            Duration::from_millis(1),
            std::num::NonZeroU32::new(2).unwrap(),
            Duration::from_millis(1),
            std::num::NonZeroU32::new(1).unwrap(),
        ),
        ..DEFAULT_READ_THROUGH_LIMITS
    };
    let (ticks, clock) = frozen_clock(0);
    let reader = reader_with_clock(&meta, &blobs, "home", delegates([("east", flaky)]), limits, clock);

    let tripped = reader.read_through(&digest).await.unwrap();
    assert_eq!(tripped, ReadThroughOutcome::Unavailable);

    let skipped = reader.read_through(&digest).await.unwrap();
    assert_eq!(skipped, ReadThroughOutcome::Unavailable);
    assert!(blobs.head(&digest).await.unwrap().is_none());

    ticks.store(61, Ordering::SeqCst);
    let recovered = reader.read_through(&digest).await.unwrap();
    assert!(matches!(recovered, ReadThroughOutcome::Served(_)));
    assert!(blobs.head(&digest).await.unwrap().is_some());
}

#[tokio::test]
async fn test_fan_out_caps_the_sources_tried() {
    let (_dir, meta, blobs) = stores();
    let content = Bytes::from_static(b"the second peer is never reached");
    let digest = Digest::of(&content);
    seed_verified(&meta, &digest, "east", "filesystem", "east/a", content.len() as u64);
    seed_verified(&meta, &digest, "west", "filesystem", "west/a", content.len() as u64);
    let down = peer(
        content.clone(),
        usize::MAX,
        TransportError::BlobNotFound {
            digest: digest.as_str().to_owned(),
        },
        Corruption::None,
    );
    let limits = ReadThroughLimits {
        max_fanout: NonZeroUsize::new(1).unwrap(),
        ..DEFAULT_READ_THROUGH_LIMITS
    };
    let reader = reader(
        &meta,
        &blobs,
        "home",
        delegates([("east", down), ("west", serving(&content))]),
        limits,
    );

    let outcome = reader.read_through(&digest).await.unwrap();

    assert_eq!(outcome, ReadThroughOutcome::Unavailable);
    assert!(blobs.head(&digest).await.unwrap().is_none());
}

#[tokio::test]
async fn test_one_source_per_data_center_even_with_several_placements() {
    let (_dir, meta, blobs) = stores();
    let content = Bytes::from_static(b"two backends, one datacenter");
    let digest = Digest::of(&content);
    seed_verified(&meta, &digest, "east", "filesystem", "east/a", content.len() as u64);
    seed_verified(&meta, &digest, "east", "s3", "east/b", content.len() as u64);
    let reader = reader(
        &meta,
        &blobs,
        "home",
        delegates([("east", serving(&content))]),
        DEFAULT_READ_THROUGH_LIMITS,
    );

    let outcome = reader.read_through(&digest).await.unwrap();

    assert!(matches!(outcome, ReadThroughOutcome::Served(_)));
}

#[tokio::test]
async fn test_serves_through_a_capacity_limited_delegate() {
    let (_dir, meta, blobs) = stores();
    let content = Bytes::from_static(b"bounded concurrency still serves");
    let digest = Digest::of(&content);
    seed_verified(&meta, &digest, "east", "filesystem", "east/a", content.len() as u64);
    let bounded: DcTransport = Arc::new(CapacityLimited::new(
        Peer {
            content: content.clone(),
            fail_first: AtomicUsize::new(0),
            error: TransportError::Disconnected,
            corruption: Corruption::None,
        },
        NonZeroUsize::new(1).unwrap(),
    ));
    let reader = reader(
        &meta,
        &blobs,
        "home",
        delegates([("east", bounded)]),
        DEFAULT_READ_THROUGH_LIMITS,
    );

    let outcome = reader.read_through(&digest).await.unwrap();

    assert!(matches!(outcome, ReadThroughOutcome::Served(_)));
}

#[tokio::test]
async fn test_availability_returns_stored_metadata_on_a_served_placement() {
    let (_dir, meta, blobs) = stores();
    let content = Bytes::from_static(b"filled from a peer");
    let digest = Digest::of(&content);
    seed_verified(&meta, &digest, "east", "filesystem", "east/a", content.len() as u64);
    let reader = reader(
        &meta,
        &blobs,
        "home",
        delegates([("east", serving(&content))]),
        DEFAULT_READ_THROUGH_LIMITS,
    );

    let metadata = reader.ensure_local(&digest).await.unwrap().unwrap();

    assert_eq!(metadata.bytes, content.len() as u64);
}

#[tokio::test]
async fn test_availability_returns_none_when_no_source_can_serve() {
    let (_dir, meta, blobs) = stores();
    let digest = Digest::of(b"unplaced");
    let reader = reader(&meta, &blobs, "home", delegates([]), DEFAULT_READ_THROUGH_LIMITS);

    assert!(reader.ensure_local(&digest).await.unwrap().is_none());
}

#[tokio::test]
async fn test_availability_classifies_a_local_staging_fault() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    meta.initialize_distributed_state().unwrap();
    let occupied = dir.path().join("occupied");
    std::fs::write(&occupied, b"not a directory").unwrap();
    let blobs = BlobStorage::filesystem(occupied.join("blobs"));
    let content = Bytes::from_static(b"verified but cannot be staged");
    let digest = Digest::of(&content);
    seed_verified(&meta, &digest, "east", "filesystem", "east/a", content.len() as u64);
    let reader = reader(
        &meta,
        &blobs,
        "home",
        delegates([("east", serving(&content))]),
        DEFAULT_READ_THROUGH_LIMITS,
    );

    let error = reader.ensure_local(&digest).await.unwrap_err();

    assert_eq!(error.kind(), BlobAvailabilityFailure::Storage);
}

#[test]
fn test_verified_size_reads_only_a_verified_placement() {
    let digest = Digest::of(b"sized");
    let record = |state| BlobPlacementRecord {
        key: key(&digest, "east", "filesystem", "east/a"),
        state,
        fence: 1,
        transfer_attempt: 1,
        generation: 2,
        updated_at_unix: 0,
    };
    assert_eq!(
        verified_size(&record(BlobPlacementState::Verified { size: 42 })),
        Some(42)
    );
    assert_eq!(
        verified_size(&record(BlobPlacementState::Failed {
            class: BlobPlacementFailure::SourceUnavailable,
        })),
        None
    );
}

#[test]
fn test_representative_prefers_a_retryable_failure() {
    let terminal = (0usize, TransportError::BlobNotFound { digest: "d".to_owned() });
    let retryable = (1usize, TransportError::Timeout);
    assert_eq!(representative(&[terminal.clone(), retryable]), &TransportError::Timeout);
    assert_eq!(representative(std::slice::from_ref(&terminal)), &terminal.1);
}

#[test]
fn test_read_through_errors_render() {
    let meta = ReadThroughError::Meta(MetaStore::open_existing("/nonexistent/read-through.redb").unwrap_err());
    let blob = ReadThroughError::Blob(BlobError::not_found(&Digest::of(b"x")));
    assert!(meta.to_string().contains("placements"));
    assert!(blob.to_string().contains("stage"));
}

#[test]
fn test_default_limits_match_the_constant() {
    assert_eq!(ReadThroughLimits::default(), DEFAULT_READ_THROUGH_LIMITS);
    assert_eq!(DEFAULT_READ_THROUGH_LIMITS.policy, DEFAULT_RECONNECT_POLICY);
}

#[tokio::test]
async fn test_streams_a_catalogued_blob_chunk_by_chunk() {
    let (_dir, meta, blobs) = stores();
    let content = Bytes::from_static(b"a very large archive drawn in several bounded chunks");
    let digest = Digest::of(&content);
    seed_verified(&meta, &digest, "east", "filesystem", "east/a", content.len() as u64);
    seed_chunk_digest(&meta, &digest, &chunked(&content, 8));
    let reader = reader(
        &meta,
        &blobs,
        "home",
        delegates([("east", serving(&content))]),
        DEFAULT_READ_THROUGH_LIMITS,
    );

    let outcome = reader.read_through(&digest).await.unwrap();

    assert!(matches!(outcome, ReadThroughOutcome::Served(_)));
    let stored = blobs.open(&digest, None).await.unwrap();
    assert_eq!(stored.collect(u64::MAX).await.unwrap(), content);
}

#[tokio::test]
async fn test_streaming_falls_through_per_chunk_to_a_healthy_source() {
    let (_dir, meta, blobs) = stores();
    let content = Bytes::from_static(b"one peer poisons every chunk, the other serves them");
    let digest = Digest::of(&content);
    seed_verified(&meta, &digest, "east", "filesystem", "east/a", content.len() as u64);
    seed_verified(&meta, &digest, "west", "filesystem", "west/a", content.len() as u64);
    seed_chunk_digest(&meta, &digest, &chunked(&content, 8));
    let poisoned = peer(content.clone(), 0, TransportError::Disconnected, Corruption::Content);
    let reader = reader(
        &meta,
        &blobs,
        "home",
        delegates([("east", poisoned), ("west", serving(&content))]),
        DEFAULT_READ_THROUGH_LIMITS,
    );

    let outcome = reader.read_through(&digest).await.unwrap();

    assert!(matches!(outcome, ReadThroughOutcome::Served(_)));
    let stored = blobs.open(&digest, None).await.unwrap();
    assert_eq!(stored.collect(u64::MAX).await.unwrap(), content);
}

#[tokio::test]
async fn test_streaming_is_unavailable_when_every_source_poisons_a_chunk() {
    let (_dir, meta, blobs) = stores();
    let content = Bytes::from_static(b"no source serves an honest chunk");
    let digest = Digest::of(&content);
    seed_verified(&meta, &digest, "east", "filesystem", "east/a", content.len() as u64);
    seed_chunk_digest(&meta, &digest, &chunked(&content, 8));
    let poisoned = peer(content.clone(), 0, TransportError::Disconnected, Corruption::Content);
    let reader = reader(
        &meta,
        &blobs,
        "home",
        delegates([("east", poisoned)]),
        DEFAULT_READ_THROUGH_LIMITS,
    );

    let outcome = reader.read_through(&digest).await.unwrap();

    assert_eq!(outcome, ReadThroughOutcome::Unavailable);
    assert!(blobs.head(&digest).await.unwrap().is_none());
}

#[tokio::test(start_paused = true)]
async fn test_streaming_retries_a_transient_chunk_loss_then_serves() {
    let (_dir, meta, blobs) = stores();
    let content = Bytes::from_static(b"the first chunk fetch drops, the retry lands it");
    let digest = Digest::of(&content);
    seed_verified(&meta, &digest, "east", "filesystem", "east/a", content.len() as u64);
    seed_chunk_digest(&meta, &digest, &chunked(&content, 8));
    let flaky = peer(content.clone(), 1, TransportError::Timeout, Corruption::None);
    let limits = ReadThroughLimits {
        circuit: CircuitConfig {
            trip_after: 5,
            ..DEFAULT_CIRCUIT
        },
        ..DEFAULT_READ_THROUGH_LIMITS
    };
    let reader = reader(&meta, &blobs, "home", delegates([("east", flaky)]), limits);

    let outcome = reader.read_through(&digest).await.unwrap();

    assert!(matches!(outcome, ReadThroughOutcome::Served(_)));
    let stored = blobs.open(&digest, None).await.unwrap();
    assert_eq!(stored.collect(u64::MAX).await.unwrap(), content);
}

#[tokio::test]
async fn test_streaming_gives_up_when_a_chunk_source_is_terminal() {
    let (_dir, meta, blobs) = stores();
    let content = Bytes::from_static(b"the peer denies holding the chunk");
    let digest = Digest::of(&content);
    seed_verified(&meta, &digest, "east", "filesystem", "east/a", content.len() as u64);
    seed_chunk_digest(&meta, &digest, &chunked(&content, 8));
    let missing = peer(
        content.clone(),
        usize::MAX,
        TransportError::BlobNotFound {
            digest: digest.as_str().to_owned(),
        },
        Corruption::None,
    );
    let reader = reader(
        &meta,
        &blobs,
        "home",
        delegates([("east", missing)]),
        DEFAULT_READ_THROUGH_LIMITS,
    );

    let outcome = reader.read_through(&digest).await.unwrap();

    assert_eq!(outcome, ReadThroughOutcome::Unavailable);
    assert!(blobs.head(&digest).await.unwrap().is_none());
}

#[tokio::test]
async fn test_streaming_commit_rejects_a_catalog_that_collides_with_a_source() {
    let (_dir, meta, blobs) = stores();
    let content = Bytes::from_static(b"the content the digest actually names");
    let digest = Digest::of(&content);
    let corrupt: Vec<u8> = content.iter().map(|byte| byte ^ 0xFF).collect();
    seed_verified(&meta, &digest, "east", "filesystem", "east/a", content.len() as u64);
    seed_chunk_digest(&meta, &digest, &chunked(&corrupt, 8));
    let source = peer(content.clone(), 0, TransportError::Disconnected, Corruption::Content);
    let reader = reader(
        &meta,
        &blobs,
        "home",
        delegates([("east", source)]),
        DEFAULT_READ_THROUGH_LIMITS,
    );

    let outcome = reader.read_through(&digest).await.unwrap();

    assert_eq!(outcome, ReadThroughOutcome::Unavailable);
    assert!(blobs.head(&digest).await.unwrap().is_none());
}

#[tokio::test(start_paused = true)]
async fn test_streaming_becomes_unavailable_when_the_only_source_trips_open() {
    let (_dir, meta, blobs) = stores();
    let content = Bytes::from_static(b"the only peer trips its circuit and stays open");
    let digest = Digest::of(&content);
    seed_verified(&meta, &digest, "east", "filesystem", "east/a", content.len() as u64);
    seed_chunk_digest(&meta, &digest, &chunked(&content, 8));
    let down = peer(content.clone(), usize::MAX, TransportError::Timeout, Corruption::None);
    let limits = ReadThroughLimits {
        circuit: CircuitConfig {
            trip_after: 1,
            ..DEFAULT_CIRCUIT
        },
        ..DEFAULT_READ_THROUGH_LIMITS
    };
    let reader = reader(&meta, &blobs, "home", delegates([("east", down)]), limits);

    let outcome = reader.read_through(&digest).await.unwrap();

    assert_eq!(outcome, ReadThroughOutcome::Unavailable);
    assert!(blobs.head(&digest).await.unwrap().is_none());
}

#[tokio::test]
async fn test_streaming_surfaces_a_local_commit_fault() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    meta.initialize_distributed_state().unwrap();
    let root = dir.path().join("blobs");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("sha256"), b"not a directory").unwrap();
    let blobs = BlobStorage::filesystem(&root);
    let content = Bytes::from_static(b"verified chunks that cannot be published");
    let digest = Digest::of(&content);
    seed_verified(&meta, &digest, "east", "filesystem", "east/a", content.len() as u64);
    seed_chunk_digest(&meta, &digest, &chunked(&content, 8));
    let reader = reader(
        &meta,
        &blobs,
        "home",
        delegates([("east", serving(&content))]),
        DEFAULT_READ_THROUGH_LIMITS,
    );

    let err = reader.read_through(&digest).await.unwrap_err();

    assert!(matches!(err, ReadThroughError::Blob(_)));
}

#[tokio::test]
async fn test_an_uncatalogued_fetch_records_the_chunk_digests_for_the_next_fetch() {
    let (_dir, meta, blobs) = stores();
    let content = Bytes::from_static(b"the first fetch streams the ranges, and records their chunk digests");
    let digest = Digest::of(&content);
    seed_verified(&meta, &digest, "east", "filesystem", "east/a", content.len() as u64);
    let reader = reader(
        &meta,
        &blobs,
        "home",
        delegates([("east", serving(&content))]),
        DEFAULT_READ_THROUGH_LIMITS,
    );
    assert!(stored_chunk_digest(&meta, &digest).is_none());

    let outcome = reader.read_through(&digest).await.unwrap();

    assert!(matches!(outcome, ReadThroughOutcome::Served(_)));
    assert_eq!(
        stored_chunk_digest(&meta, &digest),
        Some(ChunkedDigest::of(&content, CHUNK_BYTES))
    );
}

#[tokio::test]
async fn test_an_uncatalogued_fetch_serves_even_when_the_catalog_write_fails() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let content = Bytes::from_static(b"served despite a read-only catalog");
    let digest = Digest::of(&content);
    {
        let writable = MetaStore::open(&path).unwrap();
        writable.initialize_distributed_state().unwrap();
        seed_verified(&writable, &digest, "east", "filesystem", "east/a", content.len() as u64);
    }
    let meta = MetaStore::open_existing_read_only(&path).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let reader = reader(
        &meta,
        &blobs,
        "home",
        delegates([("east", serving(&content))]),
        DEFAULT_READ_THROUGH_LIMITS,
    );

    let outcome = reader.read_through(&digest).await.unwrap();

    assert!(matches!(outcome, ReadThroughOutcome::Served(_)));
    assert!(blobs.head(&digest).await.unwrap().is_some());
}

#[tokio::test]
async fn test_a_catalogued_fetch_keeps_the_stored_chunk_digests() {
    let (_dir, meta, blobs) = stores();
    let content = Bytes::from_static(b"the stored boundaries outlive the fetch that used them");
    let digest = Digest::of(&content);
    seed_verified(&meta, &digest, "east", "filesystem", "east/a", content.len() as u64);
    seed_chunk_digest(&meta, &digest, &chunked(&content, 8));
    let reader = reader(
        &meta,
        &blobs,
        "home",
        delegates([("east", serving(&content))]),
        DEFAULT_READ_THROUGH_LIMITS,
    );

    let outcome = reader.read_through(&digest).await.unwrap();

    assert!(matches!(outcome, ReadThroughOutcome::Served(_)));
    assert_eq!(stored_chunk_digest(&meta, &digest), Some(chunked(&content, 8)));
}
