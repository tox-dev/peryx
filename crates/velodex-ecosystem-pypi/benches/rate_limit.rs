//! Hot-path rate-limit benchmarks for a single resolver reading a warm mirror page.
#![allow(
    clippy::significant_drop_tightening,
    reason = "criterion_group! expands to a temporary the nursery lint flags"
)]

use std::sync::Arc;

use axum::body::Body;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use http::Request;
use http_body_util::BodyExt as _;
use tokio::runtime::Runtime;
use tower::ServiceExt as _;
use velodex_ecosystem_pypi::{Meta, ProjectDetail, to_json};
use velodex_http::rate_limit::{RateLimitConfig, RouteLimit};
use velodex_http::{AppState, Index, IndexKind, router};
use velodex_policy::Policy;
use velodex_storage::blob::BlobStore;
use velodex_storage::meta::{CachedIndex, MetaStore};
use velodex_upstream::UpstreamClient;

fn runtime() -> Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn mirror(rate_limit: RateLimitConfig) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    let body = to_json(&ProjectDetail {
        meta: Meta::default(),
        name: "flask".to_owned(),
        versions: vec!["1.0".to_owned()],
        files: Vec::new(),
    });
    meta.put_index(
        "pypi/flask",
        &CachedIndex {
            etag: None,
            last_serial: None,
            fetched_at_unix: 1000,
            content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
            fresh_secs: Some(3600),
            body: body.into_bytes(),
        },
    )
    .unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let upstream = UpstreamClient::new("http://127.0.0.1:9/simple/").unwrap();
    let mut state = AppState::with_limits(
        meta,
        blobs,
        3600,
        vec![Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: velodex_format::Ecosystem::Pypi,
            kind: IndexKind::Cached {
                client: upstream,
                offline: false,
            },
            policy: Policy::default(),
        }],
        Arc::new(|| 1000),
        rate_limit,
        [("pypi".to_owned(), 0)],
    );
    velodex_ecosystem_pypi::install(&mut state);
    (dir, Arc::new(state))
}

const fn enabled_limits() -> RateLimitConfig {
    RateLimitConfig {
        simple: RouteLimit::new(u64::MAX, 60),
        ..RateLimitConfig::enabled_defaults()
    }
}

async fn get(state: &Arc<AppState>) {
    let request = Request::builder()
        .uri("/pypi/simple/flask/")
        .header("accept", "application/vnd.pypi.simple.v1+json")
        .body(Body::empty())
        .unwrap();
    let response = router(state.clone()).oneshot(request).await.unwrap();
    assert!(response.status().is_success(), "{}", response.status());
    let _ = response.into_body().collect().await.unwrap().to_bytes();
}

fn bench_hot_simple_page(c: &mut Criterion) {
    let rt = runtime();
    let mut group = c.benchmark_group("rate_limit_hot_simple_page");
    for (name, rate_limit) in [("disabled", RateLimitConfig::default()), ("enabled", enabled_limits())] {
        let (_dir, state) = mirror(rate_limit);
        rt.block_on(get(&state));
        group.bench_with_input(BenchmarkId::from_parameter(name), &state, |b, state| {
            b.to_async(&rt).iter(|| get(state));
        });
    }
    group.finish();
}

criterion_group!(benches, bench_hot_simple_page);
criterion_main!(benches);
