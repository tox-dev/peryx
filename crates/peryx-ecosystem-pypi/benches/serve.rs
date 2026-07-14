#![allow(
    clippy::significant_drop_tightening,
    reason = "criterion_group! expands to a temporary flagged by this nursery lint"
)]

#[path = "support/detail.rs"]
mod detail;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use http::Request;
use http_body_util::BodyExt as _;
use peryx_driver::AppState;
use peryx_driver::rate_limit::{RateLimitConfig, RouteLimit};
use peryx_ecosystem_pypi::ProjectDetail;
use peryx_ecosystem_pypi::store::CachedIndex;
use peryx_ecosystem_pypi::store::PypiStore as _;
use peryx_ecosystem_pypi::to_json;
use peryx_http::router;
use peryx_identity::IndexAcl;
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;
use peryx_upstream::UpstreamClient;
use tokio::runtime::Runtime;
use tower::ServiceExt as _;

use detail::project_detail;

const LARGE: usize = 400;
const JSON: &str = "application/vnd.pypi.simple.v1+json";
const HTML: &str = "text/html";

/// Rebuilding the production-style router per iteration would hide serving cost.
fn bench_serve(criterion: &mut Criterion) {
    let runtime = runtime();
    let mut group = criterion.benchmark_group("serve");
    let detail = project_detail("flask", LARGE);
    for (name, rate_limit) in [("disabled", RateLimitConfig::default()), ("enabled", enabled_limits())] {
        let (_dir, state) = cached(rate_limit, &detail);
        let app = router(state);
        runtime.block_on(serve(app.clone(), "/pypi/simple/flask/", JSON, None));
        group.bench_with_input(BenchmarkId::new("simple_json", name), &app, |bencher, app| {
            bencher
                .to_async(&runtime)
                .iter(|| serve(app.clone(), "/pypi/simple/flask/", JSON, None));
        });
        group.bench_with_input(BenchmarkId::new("simple_html", name), &app, |bencher, app| {
            bencher
                .to_async(&runtime)
                .iter(|| serve(app.clone(), "/pypi/simple/flask/", HTML, None));
        });
        group.bench_with_input(BenchmarkId::new("legacy_json", name), &app, |bencher, app| {
            bencher
                .to_async(&runtime)
                .iter(|| serve(app.clone(), "/pypi/flask/json", JSON, None));
        });
    }
    let (_dir, state) = cached(enabled_limits(), &detail);
    let app = router(state);
    let authorization = Some("Basic cGlwOnNlY3JldA==");
    runtime.block_on(serve(app.clone(), "/pypi/simple/flask/", JSON, authorization));
    group.bench_with_input(
        BenchmarkId::new("simple_json", "enabled_authenticated"),
        &app,
        |bencher, app| {
            bencher
                .to_async(&runtime)
                .iter(|| serve(app.clone(), "/pypi/simple/flask/", JSON, authorization));
        },
    );
    let (_dir, state) = cached(trusted_proxy_limits(), &detail);
    let app = router(state);
    runtime.block_on(serve_from_proxy(app.clone(), "/pypi/simple/flask/", JSON));
    group.bench_with_input(
        BenchmarkId::new("simple_json", "enabled_trusted_proxy"),
        &app,
        |bencher, app| {
            bencher
                .to_async(&runtime)
                .iter(|| serve_from_proxy(app.clone(), "/pypi/simple/flask/", JSON));
        },
    );
    group.finish();
}

fn runtime() -> Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn cached(rate_limit: RateLimitConfig, detail: &ProjectDetail) -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    meta.put_index(
        &format!("pypi/{}", detail.name),
        &CachedIndex {
            etag: None,
            last_serial: None,
            fetched_at_unix: 1000,
            content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
            fresh_secs: Some(3600),
            body: to_json(detail).into_bytes(),
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
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Cached {
                client: upstream,
                offline: false,
            },
            policy: Policy::default(),
            acl: IndexAcl::upload_token("secret"),
        }],
        Arc::new(|| 1000),
        rate_limit,
        [("pypi".to_owned(), 0)],
    );
    peryx_ecosystem_pypi::install(&mut state);
    (dir, Arc::new(state))
}

fn enabled_limits() -> RateLimitConfig {
    RateLimitConfig {
        listing: RouteLimit::new(u64::MAX, 60),
        ..RateLimitConfig::enabled_defaults()
    }
}

fn trusted_proxy_limits() -> RateLimitConfig {
    RateLimitConfig {
        trusted_proxies: vec!["127.0.0.1/32".parse().unwrap()],
        ..enabled_limits()
    }
}

async fn serve(app: axum::Router, uri: &str, accept: &str, authorization: Option<&str>) {
    let request = Request::builder().uri(uri).header("accept", accept);
    let request = if let Some(authorization) = authorization {
        request.header("authorization", authorization)
    } else {
        request
    }
    .body(Body::empty())
    .unwrap();
    send(app, request).await;
}

async fn serve_from_proxy(app: axum::Router, uri: &str, accept: &str) {
    let mut request = Request::builder()
        .uri(uri)
        .header("accept", accept)
        .header("x-forwarded-for", "192.0.2.1")
        .body(Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 5000))));
    send(app, request).await;
}

async fn send(app: axum::Router, request: Request<Body>) {
    let response = app.oneshot(request).await.unwrap();
    assert!(response.status().is_success(), "{}", response.status());
    let _ = response.into_body().collect().await.unwrap().to_bytes();
}

criterion_group!(benches, bench_serve);
criterion_main!(benches);
