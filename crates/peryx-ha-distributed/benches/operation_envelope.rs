//! Replication-envelope encoding and decoding costs.
//!
//! Run with `cargo bench -p peryx-ha-distributed`; CI does not gate this benchmark.

use std::alloc::System;
use std::hint::black_box;
use std::time::Instant;

use hdrhistogram::Histogram;
use peryx_ha_distributed::{
    AuthorityEpoch, BlobReference, Change, DecodeLimits, MetadataMutation, OperationEnvelope, OperationKind,
};
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, Stats, StatsAlloc};

const SAMPLES: usize = 20_000;

#[global_allocator]
static ALLOCATOR: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

fn main() {
    println!("operation_envelope: benchmark");
    if std::env::args().all(|argument| argument != "--list") {
        report("single-blob", &single_blob());
        report("multi-blob", &multi_blob());
    }
}

fn report(operation: &str, envelope: &OperationEnvelope) {
    let bytes = envelope.encode();
    let encode_region = Region::new(ALLOCATOR);
    for _ in 0..SAMPLES {
        black_box(black_box(envelope).encode());
    }
    let encode_alloc = encode_region.change();
    let decode_region = Region::new(ALLOCATOR);
    for _ in 0..SAMPLES {
        black_box(OperationEnvelope::decode(black_box(&bytes), DecodeLimits::default())).unwrap();
    }
    let decode_alloc = decode_region.change();
    let encode = latency(|| {
        black_box(black_box(envelope).encode());
    });
    let decode = latency(|| {
        black_box(OperationEnvelope::decode(black_box(&bytes), DecodeLimits::default())).unwrap();
    });

    println!(
        "operation={operation} bytes={} encode_allocations={} encode_retained_bytes={} decode_allocations={} decode_retained_bytes={} encode_p50_ns={} encode_p99_ns={} decode_p50_ns={} decode_p99_ns={}",
        bytes.len(),
        per_sample(encode_alloc.allocations),
        retained_bytes(encode_alloc),
        per_sample(decode_alloc.allocations),
        retained_bytes(decode_alloc),
        encode.value_at_quantile(0.5),
        encode.value_at_quantile(0.99),
        decode.value_at_quantile(0.5),
        decode.value_at_quantile(0.99),
    );
}

fn single_blob() -> OperationEnvelope {
    OperationEnvelope::current(
        "primary-a",
        AuthorityEpoch(1),
        OperationKind::Publish,
        Change {
            serial: 4_812,
            event: vec![0x7b; 320],
            metadata: vec![MetadataMutation::Put {
                key: "adapter-a/record-4812".to_owned(),
                value: vec![0x42; 96],
            }],
            blobs: vec![BlobReference {
                sha256: "e".repeat(64),
                size: 2_374_912,
            }],
        },
    )
}

fn multi_blob() -> OperationEnvelope {
    OperationEnvelope::current(
        "primary-a",
        AuthorityEpoch(1),
        OperationKind::Publish,
        Change {
            serial: 4_813,
            event: vec![0x7b; 512],
            metadata: vec![MetadataMutation::Put {
                key: "adapter-b/record-4813".to_owned(),
                value: vec![0x42; 160],
            }],
            blobs: (0..6)
                .map(|layer| BlobReference {
                    sha256: format!("{layer:064x}"),
                    size: 8_388_608,
                })
                .collect(),
        },
    )
}

const fn per_sample(total: usize) -> usize {
    total / SAMPLES
}

fn latency(mut operation: impl FnMut()) -> Histogram<u64> {
    let mut histogram = Histogram::new(3).unwrap();
    for _ in 0..SAMPLES {
        let start = Instant::now();
        operation();
        histogram
            .record(u64::try_from(start.elapsed().as_nanos()).unwrap())
            .unwrap();
    }
    histogram
}

fn retained_bytes(stats: Stats) -> isize {
    isize::try_from(stats.bytes_allocated).unwrap() - isize::try_from(stats.bytes_deallocated).unwrap()
        + stats.bytes_reallocated
}
