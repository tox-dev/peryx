//! A first read-through of an uncatalogued blob must hold its range budget rather than the whole blob.

use std::alloc::System;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicIsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use peryx_ha::{BackendId, BackendLocation, BlobPlacementKey, BlobPlacementTransition, DataCenterId};
use peryx_ha_distributed::read_through::{
    DEFAULT_READ_THROUGH_LIMITS, DcTransport, ReadThroughLimits, ReadThroughOutcome, RemotePlacementReader,
};
use peryx_ha_distributed::{BlobRequest, BlobTransport, TransportError, apply_blob_placement};
use peryx_identity::ArtifactDigest;
use peryx_storage::blob::{BlobStorage, Digest};
use peryx_storage::meta::MetaStore;
use stats_alloc::{INSTRUMENTED_SYSTEM, StatsAlloc};

const BLOB_BYTES: usize = 32 << 20;
const RANGE_BYTES: usize = 512 << 10;
/// Four ranges in flight plus the stage's write batching, well under the blob's own size.
const MAX_RESIDENT_BYTES: isize = 8 << 20;

#[global_allocator]
static ALLOCATOR: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

/// Samples the live heap as each range is served, when the pull holds the most bytes.
struct Peer {
    content: Bytes,
    peak: Arc<AtomicIsize>,
}

#[async_trait]
impl BlobTransport for Peer {
    async fn fetch_blob(&self, request: BlobRequest) -> Result<Vec<u8>, TransportError> {
        let range = request.range.expect("a staged read-through always requests a range");
        let served = self.content.slice(range.offset..range.offset + range.length).to_vec();
        self.peak.fetch_max(resident(), Ordering::Relaxed);
        Ok(served)
    }
}

fn resident() -> isize {
    let stats = ALLOCATOR.stats();
    isize::try_from(stats.bytes_allocated).unwrap() - isize::try_from(stats.bytes_deallocated).unwrap()
}

fn seed_verified(meta: &MetaStore, digest: &Digest, size: u64) {
    let artifact = ArtifactDigest::from_sha256(digest.as_str()).unwrap();
    let key = BlobPlacementKey {
        digest: artifact.clone(),
        backend: BackendId::new("filesystem").unwrap(),
        data_center: DataCenterId::new("east").unwrap(),
        location: BackendLocation::new("east/a").unwrap(),
    };
    apply_blob_placement(meta, &key, &BlobPlacementTransition::Stage, 1, 0).unwrap();
    apply_blob_placement(
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

#[tokio::test]
async fn test_uncatalogued_read_through_holds_its_range_budget_not_the_whole_blob() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    meta.initialize_distributed_state().unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let content = Bytes::from(vec![0x5A; BLOB_BYTES]);
    let digest = Digest::of(&content);
    seed_verified(&meta, &digest, BLOB_BYTES as u64);
    let peak = Arc::new(AtomicIsize::new(isize::MIN));
    let source: DcTransport = Arc::new(Peer {
        content,
        peak: Arc::clone(&peak),
    });
    let reader = RemotePlacementReader::new(
        meta,
        blobs,
        DataCenterId::new("home").unwrap(),
        HashMap::from([("east".to_owned(), source)]),
        ReadThroughLimits {
            chunk_bytes: NonZeroUsize::new(RANGE_BYTES).unwrap(),
            ..DEFAULT_READ_THROUGH_LIMITS
        },
        Arc::new(|| 0),
    );
    let baseline = resident();

    let outcome = reader.read_through(&digest).await.unwrap();

    let observed = peak.load(Ordering::Relaxed) - baseline;
    assert_eq!(
        outcome,
        ReadThroughOutcome::Served(peryx_storage::blob::BlobMetadata {
            bytes: BLOB_BYTES as u64,
            modified: None,
        })
    );
    assert!(observed < MAX_RESIDENT_BYTES);
}
