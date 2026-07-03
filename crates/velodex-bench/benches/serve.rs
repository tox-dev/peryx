//! End-to-end serving benchmarks: a real velodex instance mirroring a temporary upstream, driven
//! through its actual axum router by a fake installer that discards every byte it receives.
//!
//! The binary half of this crate runs the full wall-clock comparison against real pypi.org and the
//! competing servers over the network, external variance and all. This instrumented half does the
//! opposite: it isolates velodex from the outside world — the upstream is a local mock serving real
//! captured `PyPI` pages and real wheels — so `CodSpeed` measures only velodex's own CPU on the
//! true request path (routing, overlay resolution, mirroring, the streaming transform, digest
//! verification, and blob persistence), and a per-commit regression is never masked by a slow CDN
//! or a noisy runner.
//!
//! Two shapes of work: serving a simple page (discovery) and installing a package (discovery plus
//! downloading its wheels and throwing them away, the way a resolver drives velodex).
#![allow(
    clippy::significant_drop_tightening,
    reason = "criterion_group! expands to a temporary the nursery lint flags"
)]

use std::fmt::Write as _;
use std::sync::Arc;

use axum::body::Body;
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use http::Request;
use http_body_util::BodyExt as _;
use tokio::runtime::Runtime;
use tokio::task::JoinSet;
use tower::ServiceExt as _;
use velodex_http::state::{Index, IndexKind};
use velodex_http::{AppState, router};
use velodex_storage::blob::{BlobStore, Digest};
use velodex_storage::meta::MetaStore;
use velodex_upstream::UpstreamClient;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Real PEP 691 pages captured from pypi.org, for the discovery benchmarks.
const PAGES: &[(&str, &str)] = &[
    ("flask", include_str!("fixtures/flask.json")),
    ("requests", include_str!("fixtures/requests.json")),
    ("numpy", include_str!("fixtures/numpy.json")),
];

/// A committed real wheel as `(filename, bytes)`, and a named set of them to install.
type Wheel = (&'static str, &'static [u8]);
type Case = (&'static str, &'static [Wheel]);

/// `(filename, bytes)` for a committed real wheel.
macro_rules! wheel {
    ($file:literal) => {
        ($file, include_bytes!(concat!("fixtures/wheels/", $file)).as_slice())
    };
}

/// A single modern package (turbohtml) for the one-wheel install.
const TURBOHTML: &[Wheel] = &[wheel!("turbohtml-0.4.0-cp315-cp315t-win_amd64.whl")];

/// Flask and its whole dependency set — seven real wheels a resolver downloads together, for the
/// concurrent multi-wheel install.
const FLASK_SET: &[Wheel] = &[
    wheel!("flask-3.1.3-py3-none-any.whl"),
    wheel!("werkzeug-3.1.8-py3-none-any.whl"),
    wheel!("jinja2-3.1.6-py3-none-any.whl"),
    wheel!("markupsafe-3.0.3-cp313-cp313t-manylinux2014_x86_64.manylinux_2_17_x86_64.manylinux_2_28_x86_64.whl"),
    wheel!("itsdangerous-2.2.0-py3-none-any.whl"),
    wheel!("click-8.4.2-py3-none-any.whl"),
    wheel!("blinker-1.9.0-py3-none-any.whl"),
];

fn runtime() -> Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
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

/// Drive one GET through the overlay router and throw the body away, the way a resolver reads it.
async fn get(state: &Arc<AppState>, uri: &str) {
    let request = Request::builder()
        .uri(uri)
        .header("accept", "application/vnd.pypi.simple.v1+json")
        .body(Body::empty())
        .unwrap();
    let response = router(state.clone()).oneshot(request).await.unwrap();
    assert!(response.status().is_success(), "{uri} -> {}", response.status());
    let _ = response.into_body().collect().await.unwrap().to_bytes();
}

// --- discovery: serve a simple page ---------------------------------------------------------------

/// A mock upstream serving each captured page at its `/simple/<project>/` path.
async fn page_upstream() -> MockServer {
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

/// Warm serve: the page is cached, so this is pure in-process CPU — routing, overlay resolution,
/// and a transformed-page memory hit. The common case by far.
fn bench_serve_warm(c: &mut Criterion) {
    let rt = runtime();
    let server = rt.block_on(page_upstream());
    let mut group = c.benchmark_group("serve_page_warm");
    for &(name, page) in PAGES {
        let uri = format!("/root/pypi/simple/{name}/");
        let (_dir, state) = mirror(&server.uri());
        rt.block_on(get(&state, &uri)); // prime
        group.throughput(Throughput::Bytes(page.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(name), &(state, uri), |b, (state, uri)| {
            b.to_async(&rt).iter(|| get(state, uri));
        });
    }
    group.finish();
}

/// Cold serve: a fresh velodex per iteration fetches the page from the upstream, transforms and
/// caches it, then serves it — the full cache-miss path an installer hits on first contact.
fn bench_serve_cold(c: &mut Criterion) {
    let rt = runtime();
    let server = rt.block_on(page_upstream());
    let uri = server.uri();
    let mut group = c.benchmark_group("serve_page_cold");
    for &(name, page) in PAGES {
        group.throughput(Throughput::Bytes(page.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(name), &uri, |b, uri| {
            b.to_async(&rt).iter_batched(
                || mirror(uri),
                |(_dir, state)| async move { get(&state, &format!("/root/pypi/simple/{name}/")).await },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

// --- install: discover, then download every wheel and discard it ---------------------------------

/// A mock upstream serving a resolution manifest for `project` plus each of its wheels. The manifest
/// points every file at this mock and carries the wheel's real sha256, so velodex fetches, verifies,
/// and caches offline exactly as it would against pypi.org.
async fn wheel_upstream(project: &str, wheels: &[Wheel]) -> (MockServer, Vec<String>) {
    let server = MockServer::start().await;
    let mut manifest = format!(r#"{{"meta":{{"api-version":"1.1"}},"name":"{project}","versions":[],"files":["#);
    let mut file_routes = Vec::new();
    for (index, &(name, bytes)) in wheels.iter().enumerate() {
        let sha = Digest::of(bytes).as_str().to_owned();
        if index > 0 {
            manifest.push(',');
        }
        let _ = write!(
            manifest,
            r#"{{"filename":"{name}","url":"{uri}/w/{name}","hashes":{{"sha256":"{sha}"}}}}"#,
            uri = server.uri(),
        );
        Mock::given(method("GET"))
            .and(path(format!("/w/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.to_vec()))
            .mount(&server)
            .await;
        file_routes.push(format!("/root/pypi/files/{sha}/{name}"));
    }
    manifest.push_str("]}");
    Mock::given(method("GET"))
        .and(path(format!("/simple/{project}/")))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(manifest.into_bytes(), "application/vnd.pypi.simple.v1+json"),
        )
        .mount(&server)
        .await;
    (server, file_routes)
}

/// The fake installer: discover the project's files, then download every wheel concurrently and
/// throw the bytes away — the resolver's full interaction with velodex.
async fn install(state: &Arc<AppState>, project: &str, file_routes: &[String]) {
    get(state, &format!("/root/pypi/simple/{project}/")).await;
    let mut downloads = JoinSet::new();
    for route in file_routes {
        let state = state.clone();
        let route = route.clone();
        downloads.spawn(async move { get(&state, &route).await });
    }
    while let Some(joined) = downloads.join_next().await {
        joined.unwrap();
    }
}

fn install_bytes(wheels: &[Wheel]) -> u64 {
    wheels.iter().map(|&(_, bytes)| bytes.len() as u64).sum()
}

/// Warm install: every wheel already cached on disk, so this prices velodex serving cached blobs
/// through the router under a concurrent download.
fn bench_install_warm(c: &mut Criterion) {
    let rt = runtime();
    let cases: &[Case] = &[("turbohtml", TURBOHTML), ("flask", FLASK_SET)];
    let mut group = c.benchmark_group("install_warm");
    for &(project, wheels) in cases {
        let (server, routes) = rt.block_on(wheel_upstream(project, wheels));
        let (_dir, state) = mirror(&server.uri());
        rt.block_on(install(&state, project, &routes)); // prime the cache
        group.throughput(Throughput::Bytes(install_bytes(wheels)));
        group.bench_with_input(
            BenchmarkId::from_parameter(project),
            &(state, routes),
            |b, (state, routes)| {
                b.to_async(&rt).iter(|| install(state, project, routes));
            },
        );
    }
    group.finish();
}

/// Cold install: a fresh velodex per iteration fetches the manifest, then downloads, verifies, and
/// persists every wheel — the full first-contact install path, including concurrent blob streaming.
fn bench_install_cold(c: &mut Criterion) {
    let rt = runtime();
    let cases: &[Case] = &[("turbohtml", TURBOHTML), ("flask", FLASK_SET)];
    let mut group = c.benchmark_group("install_cold");
    for &(project, wheels) in cases {
        let (server, routes) = rt.block_on(wheel_upstream(project, wheels));
        let uri = server.uri();
        group.throughput(Throughput::Bytes(install_bytes(wheels)));
        group.bench_with_input(
            BenchmarkId::from_parameter(project),
            &(uri, routes),
            |b, (uri, routes)| {
                b.to_async(&rt).iter_batched(
                    || mirror(uri),
                    |(_dir, state)| async move { install(&state, project, routes).await },
                    BatchSize::SmallInput,
                );
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_serve_warm,
    bench_serve_cold,
    bench_install_warm,
    bench_install_cold,
);
criterion_main!(benches);
