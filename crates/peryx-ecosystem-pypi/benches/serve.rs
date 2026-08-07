#![allow(
    clippy::significant_drop_tightening,
    reason = "criterion_group! expands to a temporary flagged by this nursery lint"
)]

#[path = "support/detail.rs"]
mod detail;

use std::hint::black_box;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use axum::body::Body;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use http::Request;
use http_body_util::BodyExt as _;
use peryx_driver::AppState;
use peryx_driver::rate_limit::{RateLimitConfig, RateLimiter, RouteClass, RouteLimit};
use peryx_ecosystem_pypi::ProjectDetail;
use peryx_ecosystem_pypi::store::CachedIndex;
use peryx_ecosystem_pypi::store::PypiStore as _;
use peryx_ecosystem_pypi::to_json;
use peryx_http::router;
use peryx_identity::{Action, Glob, Grant, IndexAcl, NamedToken};
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

fn writer_acl(secret: impl Into<String>) -> IndexAcl {
    IndexAcl {
        anonymous_read: true,
        tokens: vec![NamedToken {
            name: "uploader".to_owned(),
            secret: secret.into(),
            grants: vec![Grant {
                projects: vec![Glob::new("*")],
                actions: std::collections::BTreeSet::from([Action::Write, Action::Delete]),
            }],
            expires_at: None,
        }],
    }
}

/// Serving cost with rate limiting off. The limiter's per-request cost is measured on its own by
/// [`bench_rate_limit`]: driving it through this async router instead let scheduling jitter under the
/// instrumented runtime dominate the reading, so the `enabled` variant swung by double digits between
/// otherwise identical runs.
///
/// Rebuilding the production-style router per iteration would hide serving cost.
fn bench_serve(criterion: &mut Criterion) {
    let runtime = runtime();
    let mut group = criterion.benchmark_group("serve");
    let detail = project_detail("flask", LARGE);
    let (_dir, state) = cached(RateLimitConfig::default(), &detail);
    let app = router(state);
    runtime.block_on(serve(app.clone(), "/pypi/simple/flask/", JSON));
    group.bench_with_input(BenchmarkId::new("simple_json", "disabled"), &app, |bencher, app| {
        bencher
            .to_async(&runtime)
            .iter(|| serve(app.clone(), "/pypi/simple/flask/", JSON));
    });
    group.bench_with_input(BenchmarkId::new("simple_html", "disabled"), &app, |bencher, app| {
        bencher
            .to_async(&runtime)
            .iter(|| serve(app.clone(), "/pypi/simple/flask/", HTML));
    });
    group.bench_with_input(BenchmarkId::new("legacy_json", "disabled"), &app, |bencher, app| {
        bencher
            .to_async(&runtime)
            .iter(|| serve(app.clone(), "/pypi/flask/json", JSON));
    });
    group.finish();
}

/// Charged on every served request, so the limiter's steady-state decision is what it adds to the hot
/// path. Measured synchronously on a warm bucket, the instruction count is deterministic; a batch
/// amortizes moka's periodic housekeeping so a stray maintenance pass stays in the noise.
fn bench_rate_limit(criterion: &mut Criterion) {
    const BATCH: usize = 1024;
    let limiter = RateLimiter::new(enabled_limits());
    let client = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
    // Warm the bucket so every measured decision is a cache hit, not the one-time insert.
    let _ = limiter.check_client(RouteClass::Listing, client);
    criterion.bench_function("rate_limit_decision", |bencher| {
        bencher.iter(|| {
            for _ in 0..BATCH {
                black_box(limiter.check_client(black_box(RouteClass::Listing), black_box(client)));
            }
        });
    });
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
            ecosystem: peryx_ecosystem_pypi::ECOSYSTEM,
            kind: IndexKind::Cached {
                client: upstream,
                offline: false,
            },
            policy: Policy::default(),
            acl: writer_acl("secret"),
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

async fn serve(app: axum::Router, uri: &str, accept: &str) {
    let request = Request::builder()
        .uri(uri)
        .header("accept", accept)
        .body(Body::empty())
        .unwrap();
    send(app, request).await;
}

async fn send(app: axum::Router, request: Request<Body>) {
    let response = app.oneshot(request).await.unwrap();
    assert!(response.status().is_success(), "{}", response.status());
    let _ = response.into_body().collect().await.unwrap().to_bytes();
}

criterion_group!(benches, bench_serve, bench_rate_limit);
criterion_main!(benches);
