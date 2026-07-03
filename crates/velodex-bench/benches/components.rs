//! Component microbenchmarks: the pure, CPU-bound work velodex does on the hot path, measured in
//! isolation so `CodSpeed`'s simulation instrument reads them deterministically on shared CI.
//!
//! These are the instrumented subset of the benchmark suite whose wall-clock half — the six-server
//! comparison — lives in this crate's binary. The inputs are real `PyPI` simple pages captured from
//! pypi.org — small pure-Python `flask`, medium `requests`, and large `numpy` (a C-extension with
//! 4056 platform wheels) — plus real PEP 658 metadata for both a pure and a C-extension package. A
//! regression in parsing, transforming, serializing, versioning, or hashing shows up per commit
//! against the same shape of data the docs benchmark installs.
#![allow(
    clippy::significant_drop_tightening,
    reason = "criterion_group! expands to a temporary the nursery lint flags"
)]

use std::collections::HashMap;
use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use url::Url;
use velodex_core::pypi::{Meta, ProjectDetail, parse_detail, parse_detail_html, parse_metadata, sorted_desc, to_json};
use velodex_http::stream::{PageTransformer, page_context};
use velodex_storage::blob::Digest;

/// Real PEP 691 JSON simple pages captured from pypi.org, spanning the size range velodex serves.
const JSON_PAGES: &[(&str, &str)] = &[
    ("flask", include_str!("fixtures/flask.json")),
    ("requests", include_str!("fixtures/requests.json")),
    ("numpy", include_str!("fixtures/numpy.json")),
];

/// The same small and medium projects' real PEP 503 HTML pages, the format some indexes still serve.
const HTML_PAGES: &[(&str, &str)] = &[
    ("flask", include_str!("fixtures/flask.html")),
    ("requests", include_str!("fixtures/requests.html")),
];

/// Real PEP 658 core-metadata: a pure-Python package (Flask) and a C-extension one (numpy, more
/// dependencies and classifiers).
const METADATA: &[(&str, &str)] = &[
    ("flask", include_str!("fixtures/flask.metadata")),
    ("numpy", include_str!("fixtures/numpy.metadata")),
];

/// Turn a real page into the owned model `to_json` serializes.
fn detail_of(json: &str) -> ProjectDetail {
    let parsed = parse_detail(json.as_bytes()).unwrap();
    ProjectDetail {
        meta: Meta::default(),
        name: parsed.name,
        versions: parsed.versions,
        files: parsed.files,
    }
}

/// Parse an upstream JSON page: the first CPU step of every cache miss.
fn bench_parse_json(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse_detail_json");
    for &(name, page) in JSON_PAGES {
        group.throughput(Throughput::Bytes(page.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(name), page, |b, page| {
            b.iter(|| parse_detail(black_box(page.as_bytes())).unwrap());
        });
    }
    group.finish();
}

/// Parse an upstream HTML page: the same step for indexes that only speak PEP 503.
fn bench_parse_html(c: &mut Criterion) {
    let base = Url::parse("https://pypi.org/simple/sample/").unwrap();
    let mut group = c.benchmark_group("parse_detail_html");
    for &(name, page) in HTML_PAGES {
        group.throughput(Throughput::Bytes(page.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(name), page, |b, page| {
            b.iter(|| parse_detail_html(black_box(name), black_box(page), black_box(&base)));
        });
    }
    group.finish();
}

/// Serialize the local page model back to PEP 691 JSON: the last CPU step before the client.
fn bench_serialize(c: &mut Criterion) {
    let mut group = c.benchmark_group("to_json");
    for &(name, page) in JSON_PAGES {
        let detail = detail_of(page);
        group.bench_with_input(BenchmarkId::from_parameter(name), &detail, |b, detail| {
            b.iter(|| to_json(black_box(detail)));
        });
    }
    group.finish();
}

/// The streaming rewrite: velodex runs every cache-missed page through this, rewriting file URLs to
/// the local route and recording sources as bytes flow to the client. This is the defining hot path.
fn bench_transform(c: &mut Criterion) {
    let mut group = c.benchmark_group("page_transform");
    for &(name, page) in JSON_PAGES {
        group.throughput(Throughput::Bytes(page.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(name), page, |b, page| {
            b.iter(|| {
                let mut transformer =
                    PageTransformer::new(page_context("root/pypi", Vec::new(), Vec::new(), &HashMap::new()));
                let mut sink = Vec::new();
                for chunk in black_box(page.as_bytes()).chunks(64 * 1024) {
                    sink.extend_from_slice(&transformer.push(chunk).unwrap());
                }
                black_box((sink, transformer.finish().unwrap()));
            });
        });
    }
    group.finish();
}

/// Sort a project's versions newest-first: the PEP 440 ordering applied when merging overlay layers.
/// The input is numpy's real version list.
fn bench_version_sort(c: &mut Criterion) {
    let versions = parse_detail(JSON_PAGES[2].1.as_bytes()).unwrap().versions;
    let name = format!("version_sort/{}", versions.len());
    c.bench_function(&name, |b| {
        b.iter(|| sorted_desc(black_box(&versions)));
    });
}

/// Parse a PEP 658 core-metadata document: served on the resolver fast path so uv skips the wheel.
fn bench_metadata(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse_metadata");
    for &(name, doc) in METADATA {
        group.bench_with_input(BenchmarkId::from_parameter(name), doc, |b, doc| {
            b.iter(|| parse_metadata(black_box(doc)));
        });
    }
    group.finish();
}

/// Hash an artifact's bytes: velodex verifies every cached blob against the promised sha256. The
/// inputs are the real captured pages, standing in for artifact bytes (SHA-256 cost is per-byte).
fn bench_digest(c: &mut Criterion) {
    let mut group = c.benchmark_group("digest");
    for &(name, page) in &[("flask", JSON_PAGES[0].1), ("numpy", JSON_PAGES[2].1)] {
        group.throughput(Throughput::Bytes(page.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(name), page, |b, page| {
            b.iter(|| Digest::of(black_box(page.as_bytes())));
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_parse_json,
    bench_parse_html,
    bench_serialize,
    bench_transform,
    bench_version_sort,
    bench_metadata,
    bench_digest,
);
criterion_main!(benches);
