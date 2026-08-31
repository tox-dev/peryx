//! A ranged pull must hold its byte budget rather than the blob it is transferring.

use std::alloc::System;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicIsize, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use peryx_ha_distributed::{BlobRequest, BlobTransport, RangedPullBudget, TransportError, pull_blob_staged};
use peryx_storage::blob::{BlobStorage, Digest};
use stats_alloc::{INSTRUMENTED_SYSTEM, StatsAlloc};

const BLOB_BYTES: usize = 32 << 20;
const RANGE_BYTES: usize = 512 << 10;
const RESIDENT_BUDGET: usize = 2 << 20;
/// The scheduler's four ranges plus the stage's write batching, well under the blob's own size.
const MAX_RESIDENT_BYTES: isize = 8 << 20;

#[global_allocator]
static ALLOCATOR: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

/// Samples the live heap as each range is served, when the scheduler holds the most bytes.
struct Peer {
    content: Bytes,
    peak: Arc<AtomicIsize>,
}

#[async_trait]
impl BlobTransport for Peer {
    async fn fetch_blob(&self, request: BlobRequest) -> Result<Vec<u8>, TransportError> {
        let range = request.range.expect("a staged pull always requests a range");
        let served = self.content.slice(range.offset..range.offset + range.length).to_vec();
        self.peak.fetch_max(resident(), Ordering::Relaxed);
        Ok(served)
    }
}

fn resident() -> isize {
    let stats = ALLOCATOR.stats();
    isize::try_from(stats.bytes_allocated).unwrap() - isize::try_from(stats.bytes_deallocated).unwrap()
}

#[tokio::test]
async fn test_pull_blob_staged_holds_its_byte_budget_not_the_whole_blob() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let content = Bytes::from(vec![0x5A; BLOB_BYTES]);
    let digest = Digest::of(&content);
    let peak = Arc::new(AtomicIsize::new(isize::MIN));
    let source = Peer {
        content,
        peak: Arc::clone(&peak),
    };
    let budget = RangedPullBudget {
        range_bytes: NonZeroUsize::new(RANGE_BYTES).unwrap(),
        max_in_flight: NonZeroUsize::new(4).unwrap(),
        max_resident_bytes: NonZeroUsize::new(RESIDENT_BUDGET).unwrap(),
    };
    let baseline = resident();

    let staged = pull_blob_staged(&blobs, &[&source], &digest, BLOB_BYTES, None, budget)
        .await
        .unwrap();

    let observed = peak.load(Ordering::Relaxed) - baseline;
    assert_eq!(staged.receipt.size, BLOB_BYTES as u64);
    assert!(observed < MAX_RESIDENT_BYTES);
}
