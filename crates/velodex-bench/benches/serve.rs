//! End-to-end serving benchmarks: a real velodex instance mirroring a temporary upstream, driven
//! through its actual axum router by a fake installer that discards every byte it receives.
//!
//! The binary half of this crate runs the full wall-clock comparison against real pypi.org and the
//! competing servers over the network, external variance and all. This instrumented half does the
//! opposite: it isolates velodex from the outside world — the upstream is a local mock serving real
//! captured `PyPI` pages — so `CodSpeed` measures only velodex's own CPU on the true request path
//! (routing, overlay resolution, mirroring, the streaming transform, serialization) and a
//! per-commit regression is never masked by a slow CDN or a noisy runner.
//!
//! `CodSpeed`'s simulation instrument counts CPU on a virtual machine, so the loopback fetch on the
//! cold path is deterministic enough to compare commit to commit; the warm path touches no socket.
#![allow(
    clippy::significant_drop_tightening,
    reason = "criterion_group! expands to a temporary the nursery lint flags"
)]

use std::sync::Arc;

use axum::body::Body;
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use http::Request;
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;
use velodex_http::state::{Index, IndexKind};
use velodex_http::{AppState, router};
use velodex_storage::blob::BlobStore;
use velodex_storage::meta::MetaStore;
use velodex_upstream::UpstreamClient;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Real PEP 691 pages captured from pypi.org, spanning the size range velodex serves.
const PAGES: &[(&str, &str)] = &[
    ("flask", include_str!("fixtures/flask.json")),
    ("requests", include_str!("fixtures/requests.json")),
    ("numpy", include_str!("fixtures/numpy.json")),
];

/// Start a temporary upstream that serves each fixture at its `/simple/<project>/` path, the mirror
/// velodex fetches from — a self-contained stand-in for pypi.org.
async fn upstream() -> MockServer {
    let server = MockServer::start().await;
    for &(name, page) in PAGES {
        Mock::given(method("GET"))
            .and(path(format!("/simple/{name}/")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(page.as_bytes().to_vec(), "application/vnd.pypi.simple.v1+json"),
            )
            .mount(&server)
            .await;
    }
    server
}

/// Build a fresh velodex the way it runs in production: a mirror of the upstream, a local store, and
/// an overlay layering local in front of the mirror — the topology every request resolves through.
fn mirror(upstream_uri: &str) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let client = UpstreamClient::new(&format!("{upstream_uri}/simple/")).unwrap();
    let indexes = vec![
        Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            kind: IndexKind::Mirror(client),
        },
        Index {
            name: "local".to_owned(),
            route: "local".to_owned(),
            kind: IndexKind::Local {
                upload_token: None,
                volatile: true,
            },
        },
        Index {
            name: "root/pypi".to_owned(),
            route: "root/pypi".to_owned(),
            kind: IndexKind::Overlay {
                layers: vec![1, 0],
                upload: Some(1),
            },
        },
    ];
    // An hour of freshness so a warm page never revalidates mid-benchmark.
    (dir, Arc::new(AppState::new(meta, blobs, 3600, indexes)))
}

/// The fake installer: fetch a project's page through the overlay and throw the body away, the way
/// a resolver reads it. Panics on a non-success status so a broken serve path fails loudly.
async fn fetch(state: &Arc<AppState>, project: &str) {
    let request = Request::builder()
        .uri(format!("/root/pypi/simple/{project}/"))
        .header("accept", "application/vnd.pypi.simple.v1+json")
        .body(Body::empty())
        .unwrap();
    let response = router(state.clone()).oneshot(request).await.unwrap();
    assert!(response.status().is_success(), "serve failed: {}", response.status());
    let _ = response.into_body().collect().await.unwrap().to_bytes();
}

/// Warm serve: the page is already cached, so this is pure in-process CPU — routing, overlay
/// resolution, and a transformed-page memory hit. The common case by far.
fn bench_warm(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let server = runtime.block_on(upstream());
    let mut group = c.benchmark_group("serve_page_warm");
    for &(name, page) in PAGES {
        let (_dir, state) = mirror(&server.uri());
        runtime.block_on(fetch(&state, name)); // prime the cache
        group.throughput(Throughput::Bytes(page.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(name), &state, |b, state| {
            b.to_async(&runtime).iter(|| fetch(state, name));
        });
    }
    group.finish();
}

/// Cold serve: a fresh velodex per iteration fetches the page from the upstream, transforms and
/// caches it, then serves it — the full cache-miss path an installer hits on first contact.
fn bench_cold(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let server = runtime.block_on(upstream());
    let uri = server.uri();
    let mut group = c.benchmark_group("serve_page_cold");
    for &(name, page) in PAGES {
        group.throughput(Throughput::Bytes(page.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(name), &uri, |b, uri| {
            b.to_async(&runtime).iter_batched(
                || mirror(uri),
                |(_dir, state)| async move { fetch(&state, name).await },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, bench_warm, bench_cold);
criterion_main!(benches);
