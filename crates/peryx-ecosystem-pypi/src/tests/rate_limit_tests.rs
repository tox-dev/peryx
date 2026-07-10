use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Request, StatusCode, header};
use http_body_util::BodyExt as _;
use peryx_storage::blob::{BlobStore, Digest};
use peryx_storage::meta::MetaStore;
use peryx_upstream::UpstreamClient;
use tower::ServiceExt as _;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::http_tests::detail_json;
use super::{LogCapture, field};
use peryx_http::rate_limit::{
    DEFAULT_UPSTREAM_CONCURRENCY, RateLimitConfig, RateLimiter, RouteClass, RouteLimit, UpstreamLimited, UpstreamLimits,
};
use peryx_http::router;
use peryx_http::state::{AppState, Index, IndexKind};
use peryx_policy::Policy;

struct Harness {
    _dir: tempfile::TempDir,
    server: MockServer,
    state: Arc<AppState>,
}

async fn harness(rate_limit: RateLimitConfig, upstream_concurrency: usize) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let upstream = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let ticks = Arc::new(AtomicI64::new(1000));
    let state = super::wired(AppState::with_limits(
        meta,
        blobs,
        60,
        vec![Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Cached {
                client: upstream,
                offline: false,
            },
            policy: Policy::default(),
        }],
        Arc::new(move || ticks.load(Ordering::Relaxed)),
        rate_limit,
        [("pypi".to_owned(), upstream_concurrency)],
    ));
    Harness {
        _dir: dir,
        server,
        state,
    }
}

async fn request(state: &Arc<AppState>, uri: &str, headers: &[(&str, &str)]) -> (StatusCode, HeaderMap, String) {
    let mut builder = Request::builder().uri(uri).method("GET");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    send(state, builder.body(Body::empty()).unwrap()).await
}

async fn request_with_peer(
    state: &Arc<AppState>,
    uri: &str,
    peer: std::net::SocketAddr,
) -> (StatusCode, HeaderMap, String) {
    let mut request = Request::builder().uri(uri).method("GET").body(Body::empty()).unwrap();
    request.extensions_mut().insert(ConnectInfo(peer));
    send(state, request).await
}

async fn send(state: &Arc<AppState>, request: Request<Body>) -> (StatusCode, HeaderMap, String) {
    let response = router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8_lossy(&body).into_owned())
}

fn limit_simple(requests: u64) -> RateLimitConfig {
    RateLimitConfig {
        listing: RouteLimit::new(requests, 60),
        ..RateLimitConfig::enabled_defaults()
    }
}

#[test]
fn test_rate_limit_config_returns_metadata_and_upload_limits() {
    let config = RateLimitConfig::enabled_defaults();

    assert_eq!(config.limit(RouteClass::Metadata), RouteLimit::new(1200, 60));
    assert_eq!(config.limit(RouteClass::Upload), RouteLimit::new(60, 60));
}

#[tokio::test]
async fn test_default_rate_limiter_bypasses_requests() {
    let h = harness(RateLimitConfig::default(), DEFAULT_UPSTREAM_CONCURRENCY).await;

    let (first, ..) = request(&h.state, "/pypi/simple/", &[("x-forwarded-for", "192.0.2.1")]).await;
    let (second, ..) = request(&h.state, "/pypi/simple/", &[("x-forwarded-for", "192.0.2.1")]).await;
    let (_, _, metrics) = request(&h.state, "/metrics", &[("x-forwarded-for", "192.0.2.1")]).await;

    assert_eq!((first, second), (StatusCode::OK, StatusCode::OK));
    assert!(metrics.contains("peryx_rate_limit_allowed_total{class=\"listing\"} 0"));
}

#[tokio::test]
async fn test_ip_limit_returns_retry_after_and_separates_clients() {
    let h = harness(limit_simple(1), DEFAULT_UPSTREAM_CONCURRENCY).await;

    let (first, ..) = request(&h.state, "/pypi/simple/", &[("x-forwarded-for", "192.0.2.1")]).await;
    let (second, headers, body) = request(&h.state, "/pypi/simple/", &[("x-forwarded-for", "192.0.2.1")]).await;
    let (third, ..) = request(&h.state, "/pypi/simple/", &[("x-forwarded-for", "192.0.2.2")]).await;

    assert_eq!(
        (first, second, third),
        (StatusCode::OK, StatusCode::TOO_MANY_REQUESTS, StatusCode::OK)
    );
    assert_eq!(body, "rate limit exceeded");
    let retry_after = headers[header::RETRY_AFTER].to_str().unwrap().parse::<u64>().unwrap();
    assert!((1..=60).contains(&retry_after));
}

#[tokio::test]
async fn test_peer_metadata_beats_forwarded_headers() {
    let h = harness(limit_simple(1), DEFAULT_UPSTREAM_CONCURRENCY).await;
    let peer = std::net::SocketAddr::from(([192, 0, 2, 10], 5000));

    let (first, ..) = request_with_peer(&h.state, "/pypi/simple/", peer).await;
    let (second, ..) = request_with_peer(&h.state, "/pypi/simple/", peer).await;
    let (third, ..) = request(&h.state, "/pypi/simple/", &[("x-real-ip", "192.0.2.11")]).await;

    assert_eq!(
        (first, second, third),
        (StatusCode::OK, StatusCode::TOO_MANY_REQUESTS, StatusCode::OK)
    );
}

#[tokio::test]
async fn test_authenticated_requests_use_token_bucket() {
    let h = harness(limit_simple(1), DEFAULT_UPSTREAM_CONCURRENCY).await;
    let headers = [
        ("x-forwarded-for", "192.0.2.1"),
        ("authorization", "Basic cGlwOnNlY3JldA=="),
    ];

    let (first, ..) = request(&h.state, "/pypi/simple/", &headers).await;
    let (second, ..) = request(&h.state, "/pypi/simple/", &headers).await;
    let (third, ..) = request(
        &h.state,
        "/pypi/simple/",
        &[
            ("x-forwarded-for", "192.0.2.1"),
            ("authorization", "Basic dHdpbmU6c2VjcmV0"),
        ],
    )
    .await;

    assert_eq!(
        (first, second, third),
        (StatusCode::OK, StatusCode::TOO_MANY_REQUESTS, StatusCode::OK)
    );
}

#[tokio::test]
async fn test_disabled_limit_allows_requests_and_counts_them() {
    let h = harness(limit_simple(0), DEFAULT_UPSTREAM_CONCURRENCY).await;

    let (first, ..) = request(&h.state, "/pypi/simple/", &[("x-forwarded-for", "192.0.2.1")]).await;
    let (second, ..) = request(&h.state, "/pypi/simple/", &[("x-forwarded-for", "192.0.2.1")]).await;
    let (_, _, metrics) = request(&h.state, "/metrics", &[("x-forwarded-for", "192.0.2.1")]).await;

    assert_eq!((first, second), (StatusCode::OK, StatusCode::OK));
    assert!(metrics.contains("peryx_rate_limit_allowed_total{class=\"listing\"} 2"));
}

#[tokio::test]
async fn test_window_reset_allows_requests_after_retry_after() {
    let h = harness(
        RateLimitConfig {
            listing: RouteLimit::new(1, 1),
            ..RateLimitConfig::enabled_defaults()
        },
        DEFAULT_UPSTREAM_CONCURRENCY,
    )
    .await;

    let (first, ..) = request(&h.state, "/pypi/simple/", &[("x-forwarded-for", "192.0.2.1")]).await;
    let (second, ..) = request(&h.state, "/pypi/simple/", &[("x-forwarded-for", "192.0.2.1")]).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let (third, ..) = request(&h.state, "/pypi/simple/", &[("x-forwarded-for", "192.0.2.1")]).await;

    assert_eq!(
        (first, second, third),
        (StatusCode::OK, StatusCode::TOO_MANY_REQUESTS, StatusCode::OK)
    );
}

#[tokio::test]
async fn test_rate_limit_does_not_block_listing_pages() {
    let rate_limit = RateLimitConfig {
        artifact: RouteLimit::new(1, 60),
        listing: RouteLimit::new(10, 60),
        ..RateLimitConfig::enabled_defaults()
    };
    let h = harness(rate_limit, DEFAULT_UPSTREAM_CONCURRENCY).await;
    let digest = "0".repeat(64);
    let uri = format!("/pypi/files/{digest}/flask-1.0-py3-none-any.whl");

    let (first, ..) = request(&h.state, &uri, &[("x-forwarded-for", "192.0.2.1")]).await;
    let (second, ..) = request(&h.state, &uri, &[("x-forwarded-for", "192.0.2.1")]).await;
    let (simple, ..) = request(&h.state, "/pypi/simple/", &[("x-forwarded-for", "192.0.2.1")]).await;

    assert_eq!(
        (first, second, simple),
        (StatusCode::NOT_FOUND, StatusCode::TOO_MANY_REQUESTS, StatusCode::OK)
    );
}

#[tokio::test]
async fn test_denials_are_logged_and_counted() {
    let capture = LogCapture::default();
    let _guard = capture.install();
    let h = harness(limit_simple(1), DEFAULT_UPSTREAM_CONCURRENCY).await;

    let _ = request(&h.state, "/pypi/simple/", &[("x-forwarded-for", "192.0.2.1")]).await;
    let _ = request(&h.state, "/pypi/simple/", &[("x-forwarded-for", "192.0.2.1")]).await;
    let (_, _, metrics) = request(&h.state, "/metrics", &[("x-forwarded-for", "192.0.2.1")]).await;

    assert!(metrics.contains("peryx_rate_limit_denied_total{class=\"listing\"} 1"));
    let events = capture.security_events();
    let event = events
        .iter()
        .find(|event| field(event, "event") == Some("rate_limit"))
        .unwrap_or_else(|| panic!("{}", capture.text()));
    assert_eq!(field(event, "action"), Some("http_request"));
    assert_eq!(field(event, "result"), Some("denied"));
    assert_eq!(field(event, "class"), Some("listing"));
    assert_eq!(field(event, "client"), Some("ip"));
    assert!((1..=60).contains(&event["fields"]["retry_after"].as_u64().unwrap()));
}

#[tokio::test]
async fn test_token_denials_are_logged_as_token_clients() {
    let capture = LogCapture::default();
    let _guard = capture.install();
    let h = harness(limit_simple(1), DEFAULT_UPSTREAM_CONCURRENCY).await;
    let headers = [
        ("x-forwarded-for", "192.0.2.1"),
        ("authorization", "Basic cGlwOnNlY3JldA=="),
    ];

    let _ = request(&h.state, "/pypi/simple/", &headers).await;
    let _ = request(&h.state, "/pypi/simple/", &headers).await;

    let events = capture.security_events();
    let event = events
        .iter()
        .find(|event| field(event, "client") == Some("token"))
        .unwrap_or_else(|| panic!("{}", capture.text()));
    assert_eq!(field(event, "event"), Some("rate_limit"));
    assert_eq!(field(event, "result"), Some("denied"));
}

#[tokio::test]
async fn test_upstream_concurrency_cap_applies_backpressure() {
    let h = harness(RateLimitConfig::default(), 1).await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(150))
                .set_body_raw(
                    detail_json(digest.as_str(), &file_url).into_bytes(),
                    "application/vnd.pypi.simple.v1+json",
                ),
        )
        .expect(2)
        .mount(&h.server)
        .await;

    let (first, second) = tokio::join!(
        request(&h.state, "/pypi/simple/flask/", &[("x-forwarded-for", "192.0.2.1")]),
        request(&h.state, "/pypi/simple/django/", &[("x-forwarded-for", "192.0.2.2")]),
    );

    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(second.0, StatusCode::OK);

    let (_, _, metrics) = request(&h.state, "/metrics", &[]).await;
    assert!(metrics.contains("peryx_upstream_rate_limit_denied_total{index=\"pypi\"} 0"));
}

// `held` must keep the only permit alive for the whole test so the second acquire saturates and times out.
#[expect(clippy::significant_drop_tightening)]
#[tokio::test(start_paused = true)]
async fn test_upstream_acquire_times_out_when_saturated() {
    let limits = UpstreamLimits::new([("pypi".to_owned(), 1)]);

    let held = limits.acquire("pypi").await.unwrap();
    assert!(held.is_some());

    let denied = limits.acquire("pypi").await;

    assert!(matches!(denied, Err(UpstreamLimited { retry_after: 1 })));
    assert_eq!(limits.snapshots()[0].denied, 1);
}

#[tokio::test(start_paused = true)]
async fn test_request_returns_429_when_upstream_cap_saturated() {
    let h = harness(RateLimitConfig::default(), 1).await;
    let held = h.state.upstream_limits.acquire("pypi").await.unwrap();
    assert!(held.is_some());

    let (status, headers, body) = request(&h.state, "/pypi/simple/flask/", &[]).await;
    drop(held);

    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(headers[header::RETRY_AFTER].to_str().unwrap(), "1");
    assert!(body.contains("rate limit exceeded"));
}

#[tokio::test(start_paused = true)]
async fn test_virtual_index_surfaces_429_when_only_layer_is_rate_limited() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let upstream = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let ticks = Arc::new(AtomicI64::new(1000));
    let state = super::wired(AppState::with_limits(
        meta,
        blobs,
        60,
        vec![
            Index {
                name: "pypi".to_owned(),
                route: "pypi".to_owned(),
                ecosystem: peryx_core::Ecosystem::Pypi,
                kind: IndexKind::Cached {
                    client: upstream,
                    offline: false,
                },
                policy: Policy::default(),
            },
            Index {
                name: "root".to_owned(),
                route: "root".to_owned(),
                ecosystem: peryx_core::Ecosystem::Pypi,
                kind: IndexKind::Virtual {
                    layers: vec![0],
                    upload: None,
                },
                policy: Policy::default(),
            },
        ],
        Arc::new(move || ticks.load(Ordering::Relaxed)),
        RateLimitConfig::default(),
        [("pypi".to_owned(), 1)],
    ));

    let held = state.upstream_limits.acquire("pypi").await.unwrap();
    assert!(held.is_some());

    let (status, headers, _) = request(&state, "/root/simple/flask/", &[]).await;
    drop(held);

    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(headers[header::RETRY_AFTER].to_str().unwrap(), "1");
}

#[test]
fn test_rate_limiter_default_has_zeroed_counters() {
    let limiter = RateLimiter::default();
    let counters = limiter.counters();

    assert!(!limiter.enabled());
    assert_eq!(counters.len(), 5);
    assert!(
        counters
            .iter()
            .all(|snapshot| snapshot.allowed == 0 && snapshot.denied == 0)
    );
}

#[test]
fn test_state_with_rate_limits_sets_limiter_and_upstream_cap() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let upstream = UpstreamClient::new("http://127.0.0.1:9/simple/").unwrap();
    let state = AppState::with_rate_limits(
        meta,
        blobs,
        60,
        vec![Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Cached {
                client: upstream,
                offline: false,
            },
            policy: Policy::default(),
        }],
        RateLimitConfig::enabled_defaults(),
        [("pypi".to_owned(), 1)],
    );

    let snapshots = state.upstream_limits.snapshots();

    assert!(state.rate_limits.enabled());
    assert_eq!(snapshots[0].max_concurrent, 1);
}

#[test]
fn test_state_with_search_path_uses_disabled_limiter() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let state = AppState::with_search_path(meta, blobs, 60, Vec::new(), dir.path().join("search-v1")).unwrap();

    assert!(!state.rate_limits.enabled());
    assert!(state.upstream_limits.snapshots().is_empty());
}

#[tokio::test]
async fn test_upstream_limits_allow_unknown_and_uncapped_mirrors() {
    let limits = UpstreamLimits::new([("z".to_owned(), 0), ("a".to_owned(), 2)]);

    assert!(matches!(limits.acquire("missing").await, Ok(None)));
    assert!(matches!(limits.acquire("z").await, Ok(None)));
    let snapshots = limits.snapshots();

    assert_eq!(snapshots[0].index, "a");
    assert_eq!(snapshots[0].max_concurrent, 2);
    assert_eq!(snapshots[0].in_flight, 0);
    assert_eq!(snapshots[1].index, "z");
    assert_eq!(snapshots[1].max_concurrent, 0);
    assert_eq!(snapshots[1].in_flight, 0);
}

#[test]
fn test_pypi_classify_route_distinguishes_metadata_artifact_listing() {
    use peryx_http::rate_limit::RouteClass;
    use peryx_http::serving::EcosystemServing as _;

    let driver = crate::PypiServing;
    assert_eq!(driver.classify_route("/pypi/simple/flask/"), RouteClass::Listing);
    assert_eq!(
        driver.classify_route("/pypi/files/abc/flask-1.0.whl"),
        RouteClass::Artifact
    );
    assert_eq!(
        driver.classify_route("/pypi/files/abc/flask-1.0.whl.metadata"),
        RouteClass::Metadata
    );
    assert_eq!(
        driver.classify_route("/pypi/inspect/abc/flask-1.0.whl"),
        RouteClass::Artifact
    );
}
