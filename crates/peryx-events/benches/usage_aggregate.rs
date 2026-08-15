//! Request recording and durable checkpoint costs.

use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, Throughput};
use peryx_events::metrics::{Clock, Metrics, Observation};
use peryx_storage::meta::MetaStore;

const BATCHES: [usize; 3] = [1, 64, 1024];

const CARDINALITIES: [usize; 3] = [64, 4_096, 32_768];

fn fixed_clock() -> Clock {
    Arc::new(|| 20_000 * 86_400)
}

fn batch(size: usize) -> Vec<Observation> {
    (0..size)
        .map(|index| Observation::Read {
            repository: "alpha".to_owned(),
            resource: format!("resource-{}", index % 8),
            artifact: format!("resource-{}-{}.0.bin", index % 8, index % 16),
            group: Some(format!("{}.0", index % 16)),
            source: Some("upstream".to_owned()),
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
            let metrics = Metrics::start_durable(meta.analytics(), Some(30), fixed_clock()).unwrap();
            bencher.iter(|| {
                for event in events {
                    metrics.record(event.clone());
                }
            });
        });
    }
    group.finish();
}

fn seed(count: usize) -> Vec<Observation> {
    (0..count)
        .map(|index| Observation::Read {
            repository: "alpha".to_owned(),
            resource: format!("resource-{}", index % 512),
            artifact: format!("resource-{}-{}.{}.0.bin", index % 512, index % 64, index % 16),
            group: Some(format!("{}.{}.0", index % 64, index % 16)),
            source: Some("upstream".to_owned()),
            bytes: 4096,
        })
        .collect()
}

fn bench_isolated_checkpoint(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("isolated_checkpoint");
    for cardinality in CARDINALITIES {
        group.throughput(Throughput::Elements(1));
        group.bench_with_input(
            BenchmarkId::from_parameter(cardinality),
            &cardinality,
            |bencher, &cardinality| {
                let dir = tempfile::tempdir().unwrap();
                let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
                let metrics = Metrics::start_durable(meta.analytics(), Some(366), fixed_clock()).unwrap();
                for event in seed(cardinality) {
                    metrics.record(event);
                }
                metrics.flush().unwrap();
                bencher.iter(|| {
                    metrics.record(Observation::Read {
                        repository: "alpha".to_owned(),
                        resource: "resource-0".to_owned(),
                        artifact: "resource-0-0.0.0.bin".to_owned(),
                        group: Some("0.0.0".to_owned()),
                        source: Some("upstream".to_owned()),
                        bytes: 4096,
                    });
                    metrics.flush().unwrap();
                });
            },
        );
    }
    group.finish();
}

fn main() {
    let mut criterion = Criterion::default().configure_from_args();
    bench_usage_aggregate(&mut criterion);
    bench_isolated_checkpoint(&mut criterion);
    criterion.final_summary();
}
