#![allow(
    clippy::significant_drop_tightening,
    reason = "criterion_group! expands to a temporary flagged by this nursery lint"
)]

#[path = "support/detail.rs"]
mod detail;

use std::alloc::System;
use std::time::Instant;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use hdrhistogram::Histogram;
use peryx_ecosystem_pypi::stream::{PageContext, PageTransformer};
use peryx_ecosystem_pypi::{ProjectDetail, parse_detail_html, render_detail_html, to_json};
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};
use url::Url;

use detail::project_detail;

const LATENCY_SAMPLES: usize = 1_000;

#[global_allocator]
static ALLOCATOR: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

fn bench_transform(criterion: &mut Criterion) {
    let detail = project_detail("flask", 400);
    let json = to_json(&detail).into_bytes();
    let html = render_detail_html(&detail);
    let base = Url::parse("https://pypi.org/simple/flask/").unwrap();

    report_allocations("html", || transform_html(&html, &base));
    report_allocations("json", || transform_json(&json));
    report_tail_latency("html", html.len(), || transform_html(&html, &base));
    report_tail_latency("json", json.len(), || transform_json(&json));

    let mut group = criterion.benchmark_group("transform");
    group.throughput(Throughput::Bytes(html.len() as u64));
    group.bench_function(BenchmarkId::new("html", "large"), |bencher| {
        bencher.iter(|| transform_html(std::hint::black_box(&html), &base));
    });
    group.throughput(Throughput::Bytes(json.len() as u64));
    group.bench_function(BenchmarkId::new("json", "large"), |bencher| {
        bencher.iter(|| transform_json(std::hint::black_box(&json)));
    });
    group.finish();
}

fn report_allocations(name: &str, transform: impl FnOnce() -> Vec<u8>) {
    let region = Region::new(ALLOCATOR);
    std::hint::black_box(transform());
    let stats = region.change();
    eprintln!(
        "transform/{name}: allocations={}, allocated_bytes={}, reallocations={}",
        stats.allocations, stats.bytes_allocated, stats.reallocations
    );
}

fn report_tail_latency(name: &str, input_bytes: usize, transform: impl Fn() -> Vec<u8>) {
    let mut latency = Histogram::<u64>::new(3).unwrap();
    let started = Instant::now();
    for _ in 0..LATENCY_SAMPLES {
        let sample_started = Instant::now();
        std::hint::black_box(transform());
        latency
            .record(u64::try_from(sample_started.elapsed().as_nanos()).unwrap())
            .unwrap();
    }
    let throughput = input_bytes as u128 * LATENCY_SAMPLES as u128 * 1_000_000_000 / started.elapsed().as_nanos();
    eprintln!(
        "transform/{name}: p99_ns={}, throughput_bytes_per_second={throughput}",
        latency.value_at_quantile(0.99)
    );
}

fn transform_html(html: &str, base: &Url) -> Vec<u8> {
    let parsed = parse_detail_html("flask", html, base).unwrap();
    transform_json(
        to_json(&ProjectDetail {
            meta: parsed.meta,
            name: parsed.name,
            versions: parsed.versions,
            files: parsed.files,
        })
        .as_bytes(),
    )
}

fn transform_json(json: &[u8]) -> Vec<u8> {
    let mut transformer = PageTransformer::new(PageContext::default());
    let mut out = Vec::with_capacity(json.len());
    for chunk in json.chunks(16 * 1024) {
        transformer.push_into(chunk, &mut out).unwrap();
    }
    transformer.finish().unwrap();
    out
}

criterion_group!(benches, bench_transform);
criterion_main!(benches);
