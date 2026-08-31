//! Large-layer ranged pull: throughput, retained bytes, and how the ranges spread over sources.
//!
//! Run this microbenchmark with `cargo bench -p peryx-ha-distributed`.

use std::alloc::System;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use peryx_ha_distributed::{BlobRequest, BlobTransport, RangedPullBudget, TransportError, pull_blob_staged};
use peryx_storage::blob::{BlobStorage, Digest};
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, Stats, StatsAlloc};

const BLOB_BYTES: usize = 64 << 20;
const RANGE_BYTES: usize = 1 << 20;
const RESIDENT_BUDGET: usize = 4 << 20;
const MAX_IN_FLIGHT: usize = 4;

#[global_allocator]
static ALLOCATOR: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

fn main() {
    println!("ranged_pull: benchmark");
    if std::env::args().all(|argument| argument != "--list") {
        report_pull(1);
        report_pull(3);
    }
}

struct Peer {
    content: Bytes,
    served: AtomicUsize,
    /// Bytes live on the heap when a range is served, the moment the pipeline holds the most.
    peak: AtomicIsize,
}

#[async_trait]
impl BlobTransport for Peer {
    async fn fetch_blob(&self, request: BlobRequest) -> Result<Vec<u8>, TransportError> {
        let range = request.range.expect("a staged pull always requests a range");
        let served = self.content.slice(range.offset..range.offset + range.length).to_vec();
        self.served.fetch_add(1, Ordering::Relaxed);
        self.peak.fetch_max(retained(ALLOCATOR.stats()), Ordering::Relaxed);
        Ok(served)
    }
}

fn report_pull(source_count: usize) {
    let dir = tempfile::tempdir().unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let content = Bytes::from(vec![0x5A; BLOB_BYTES]);
    let digest = Digest::of(&content);
    let peers: Vec<Peer> = (0..source_count)
        .map(|_| Peer {
            content: content.clone(),
            served: AtomicUsize::new(0),
            peak: AtomicIsize::new(isize::MIN),
        })
        .collect();
    let sources: Vec<&Peer> = peers.iter().collect();
    let budget = RangedPullBudget {
        range_bytes: NonZeroUsize::new(RANGE_BYTES).unwrap(),
        max_in_flight: NonZeroUsize::new(MAX_IN_FLIGHT).unwrap(),
        max_resident_bytes: NonZeroUsize::new(RESIDENT_BUDGET).unwrap(),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let region = Region::new(ALLOCATOR);
    let baseline = retained(ALLOCATOR.stats());
    let start = Instant::now();
    runtime
        .block_on(pull_blob_staged(&blobs, &sources, &digest, BLOB_BYTES, None, budget))
        .unwrap();
    let elapsed = start.elapsed();
    let alloc = region.change();
    let peak = peers
        .iter()
        .map(|peer| peer.peak.load(Ordering::Relaxed))
        .max()
        .unwrap_or(baseline)
        - baseline;
    let distribution: Vec<String> = peers
        .iter()
        .map(|peer| peer.served.load(Ordering::Relaxed).to_string())
        .collect();
    let throughput = (BLOB_BYTES as u128 * 1_000_000_000)
        .checked_div(elapsed.as_nanos())
        .unwrap_or(0);
    println!(
        "leg=ranged_pull sources={source_count} blob_bytes={BLOB_BYTES} range_bytes={RANGE_BYTES} \
         in_flight={MAX_IN_FLIGHT} allocations={} retained_bytes={} peak_resident_bytes={peak} \
         elapsed_ms={} bytes_per_sec={throughput} ranges_per_source={}",
        alloc.allocations,
        retained(alloc),
        elapsed.as_millis(),
        distribution.join("/"),
    );
}

fn retained(stats: Stats) -> isize {
    isize::try_from(stats.bytes_allocated).unwrap() - isize::try_from(stats.bytes_deallocated).unwrap()
        + stats.bytes_reallocated
}
