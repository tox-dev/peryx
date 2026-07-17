#![allow(
    clippy::significant_drop_tightening,
    reason = "criterion_group! expands to a temporary flagged by this nursery lint"
)]

//! What recording usage costs the request path, and how batching amortizes the durable write.
//!
//! The daily aggregate's design claim is that collection stays off the request path: a handler emits
//! one channel send, and folding, retention, and persistence happen on the aggregator thread. These
//! legs measure that claim directly.
//!
//! - `ephemeral` vs `durable` is collection disabled vs enabled: the two legs should track each other,
//!   because turning persistence on adds work to the aggregator thread, not to the measured emit.
//! - The batch dimension is the write-amplification and batching signal: the aggregator serializes one
//!   snapshot per drained batch, so a larger batch persists the same download count in fewer writes.
//!
//! The CI performance runner does not build this package's benches, so this is a local
//! `cargo bench -p peryx-events` tool; it never gates CI. Throughput and its p99 come from the
//! criterion run; peak memory is bounded by the live bucket count (repositories x projects x versions
//! x sources x retained days).

use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use peryx_events::metrics::{Clock, Event, Metrics};
use peryx_storage::meta::MetaStore;

const BATCHES: [usize; 3] = [1, 64, 1024];

/// A frozen clock: every download in a run dates to the same UTC bucket, so the fold is measured, not
/// the calendar.
fn fixed_clock() -> Clock {
    Arc::new(|| 20_000 * 86_400)
}

/// A spread of downloads across a bounded label set, the shape retention and the daily fold see in
/// production: a handful of projects, each with a few versions, from one routed source.
fn batch(size: usize) -> Vec<Event> {
    (0..size)
        .map(|index| Event::Download {
            route: "pypi".to_owned(),
            project: format!("project-{}", index % 8),
            filename: format!("project-{}-{}.0.whl", index % 8, index % 16),
            version: Some(format!("{}.0", index % 16)),
            source: Some("pypi-org".to_owned()),
            bytes: 4096,
        })
        .collect()
}

fn bench_usage_aggregate(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("usage_aggregate");
    for size in BATCHES {
        let events = batch(size);
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::new("ephemeral", size), &events, |bencher, events| {
            let metrics = Metrics::start();
            bencher.iter(|| {
                for event in events {
                    metrics.record(event.clone());
                }
            });
        });
        group.bench_with_input(BenchmarkId::new("durable", size), &events, |bencher, events| {
            let dir = tempfile::tempdir().unwrap();
            let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
            let metrics = Metrics::start_durable(meta.analytics(), Some(30), fixed_clock());
            bencher.iter(|| {
                for event in events {
                    metrics.record(event.clone());
                }
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_usage_aggregate);
criterion_main!(benches);
