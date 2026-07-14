use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, header};
use base64::Engine as _;
use http_body_util::BodyExt as _;
use peryx_storage::blob::{BlobStore, Digest};
use peryx_storage::meta::MetaStore;
use peryx_upstream::UpstreamClient;
use tower::ServiceExt as _;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::http::detail_json;
use super::{LogCapture, field};
use peryx_driver::rate_limit::{
    DEFAULT_UPSTREAM_CONCURRENCY, RateLimitConfig, RateLimiter, RouteClass, RouteLimit, UpstreamLimited, UpstreamLimits,
};
use peryx_driver::state::AppState;
use peryx_http::router;
use peryx_identity::{IndexAcl, NamedToken};
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;

struct Harness {
    _dir: tempfile::TempDir,
    server: MockServer,
    state: Arc<AppState>,
}

async fn harness(rate_limit: RateLimitConfig, upstream_concurrency: usize) -> Harness {
    harness_with_acl(rate_limit, upstream_concurrency, IndexAcl::default()).await
}

async fn request(state: &Arc<AppState>, uri: &str, headers: &[(&str, &str)]) -> (StatusCode, HeaderMap, String) {
    send(state, get_request(uri, headers)).await
}

fn get_request(uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder().uri(uri).method("GET");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::empty()).unwrap()
}

async fn request_with_peer(
    state: &Arc<AppState>,
    uri: &str,
    peer: SocketAddr,
    headers: &[(&str, &str)],
) -> (StatusCode, HeaderMap, String) {
    let mut request = get_request(uri, headers);
    request.extensions_mut().insert(ConnectInfo(peer));
    send(state, request).await
}

async fn status_with_peer(state: &Arc<AppState>, peer: SocketAddr, headers: &[(&str, &str)]) -> StatusCode {
    request_with_peer(state, "/pypi/simple/", peer, headers).await.0
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

fn limit_simple_with_trusted_proxies(requests: u64, trusted_proxies: &[&str]) -> RateLimitConfig {
    RateLimitConfig {
        trusted_proxies: trusted_proxies.iter().map(|network| network.parse().unwrap()).collect(),
        ..limit_simple(requests)
    }
}

async fn trusted_proxy_harness() -> (Harness, SocketAddr) {
    (
        harness(
            limit_simple_with_trusted_proxies(1, &["198.51.100.0/24"]),
            DEFAULT_UPSTREAM_CONCURRENCY,
        )
        .await,
        "198.51.100.10:5000".parse().unwrap(),
    )
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
async fn test_trusted_proxy_separates_clients() {
    let (harness, peer) = trusted_proxy_harness().await;

    let first = status_with_peer(&harness.state, peer, &[("x-forwarded-for", "192.0.2.1")]).await;
    let second = status_with_peer(&harness.state, peer, &[("x-forwarded-for", "192.0.2.1")]).await;
    let third = status_with_peer(&harness.state, peer, &[("x-forwarded-for", "192.0.2.2")]).await;

    assert_eq!(
        (first, second, third),
        (StatusCode::OK, StatusCode::TOO_MANY_REQUESTS, StatusCode::OK)
    );
}

#[tokio::test]
async fn test_limit_response_returns_body_and_retry_after() {
    let (harness, peer) = trusted_proxy_harness().await;
    status_with_peer(&harness.state, peer, &[("x-forwarded-for", "192.0.2.1")]).await;
    let (status, headers, body) = request_with_peer(
        &harness.state,
        "/pypi/simple/",
        peer,
        &[("x-forwarded-for", "192.0.2.1")],
    )
    .await;

    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body, "rate limit exceeded");
    let retry_after = headers[header::RETRY_AFTER].to_str().unwrap().parse::<u64>().unwrap();
    assert!((1..=60).contains(&retry_after));
}

#[tokio::test]
async fn test_forwarded_headers_without_peer_share_local_bucket() {
    let harness = harness(limit_simple(1), DEFAULT_UPSTREAM_CONCURRENCY).await;

    let (first, ..) = request(&harness.state, "/pypi/simple/", &[("x-forwarded-for", "192.0.2.1")]).await;
    let (second, ..) = request(&harness.state, "/pypi/simple/", &[("x-forwarded-for", "192.0.2.2")]).await;

    assert_eq!((first, second), (StatusCode::OK, StatusCode::TOO_MANY_REQUESTS));
}

#[tokio::test]
async fn test_untrusted_peer_ignores_forwarded_headers() {
    let harness = harness(limit_simple(1), DEFAULT_UPSTREAM_CONCURRENCY).await;
    let first_peer = SocketAddr::from(([192, 0, 2, 10], 5000));
    let second_peer = SocketAddr::from(([192, 0, 2, 11], 5000));

    let first = status_with_peer(&harness.state, first_peer, &[("x-forwarded-for", "198.51.100.1")]).await;
    let second = status_with_peer(&harness.state, first_peer, &[("x-forwarded-for", "198.51.100.2")]).await;
    let third = status_with_peer(&harness.state, second_peer, &[("x-forwarded-for", "198.51.100.1")]).await;

    assert_eq!(
        (first, second, third),
        (StatusCode::OK, StatusCode::TOO_MANY_REQUESTS, StatusCode::OK)
    );
}

#[tokio::test]
async fn test_trusted_proxy_uses_nearest_untrusted_forwarded_hop() {
    let (harness, peer) = trusted_proxy_harness().await;

    let first = status_with_peer(
        &harness.state,
        peer,
        &[("x-forwarded-for", "192.0.2.1, 203.0.113.5, 198.51.100.20")],
    )
    .await;
    let second = status_with_peer(
        &harness.state,
        peer,
        &[("x-forwarded-for", "192.0.2.2, 203.0.113.5, 198.51.100.21")],
    )
    .await;

    assert_eq!((first, second), (StatusCode::OK, StatusCode::TOO_MANY_REQUESTS));
}

#[tokio::test]
async fn test_trusted_proxy_reads_repeated_forwarded_fields_in_order() {
    let (harness, peer) = trusted_proxy_harness().await;
    let first_headers = [
        ("x-forwarded-for", "192.0.2.1"),
        ("x-forwarded-for", "203.0.113.5, 198.51.100.20"),
    ];
    let second_headers = [
        ("x-forwarded-for", "192.0.2.2"),
        ("x-forwarded-for", "203.0.113.5, 198.51.100.21"),
    ];

    let first = status_with_peer(&harness.state, peer, &first_headers).await;
    let second = status_with_peer(&harness.state, peer, &second_headers).await;

    assert_eq!((first, second), (StatusCode::OK, StatusCode::TOO_MANY_REQUESTS));
}

#[rstest::rstest]
#[case::malformed_suffix("192.0.2.1, invalid, 198.51.100.20", "192.0.2.2, invalid, 198.51.100.20")]
#[case::trusted_throughout("198.51.100.20, 198.51.100.21", "198.51.100.22")]
#[tokio::test]
async fn test_trusted_proxy_falls_back_to_peer_for_unusable_forwarded_chain(
    #[case] first_header: &str,
    #[case] second_header: &str,
) {
    let (harness, peer) = trusted_proxy_harness().await;

    let first = status_with_peer(
        &harness.state,
        peer,
        &[("x-forwarded-for", first_header), ("x-real-ip", "203.0.113.1")],
    )
    .await;
    let second = status_with_peer(
        &harness.state,
        peer,
        &[("x-forwarded-for", second_header), ("x-real-ip", "203.0.113.2")],
    )
    .await;

    assert_eq!((first, second), (StatusCode::OK, StatusCode::TOO_MANY_REQUESTS));
}

#[tokio::test]
async fn test_trusted_proxy_rejects_repeated_real_ip_fields() {
    let (harness, peer) = trusted_proxy_harness().await;

    let first = status_with_peer(
        &harness.state,
        peer,
        &[("x-real-ip", "192.0.2.1"), ("x-real-ip", "192.0.2.2")],
    )
    .await;
    let second = status_with_peer(
        &harness.state,
        peer,
        &[("x-real-ip", "192.0.2.3"), ("x-real-ip", "192.0.2.4")],
    )
    .await;

    assert_eq!((first, second), (StatusCode::OK, StatusCode::TOO_MANY_REQUESTS));
}

#[tokio::test]
async fn test_trusted_proxy_rejects_non_text_forwarded_field() {
    let (harness, peer) = trusted_proxy_harness().await;
    let request = || {
        let mut request = get_request("/pypi/simple/", &[]);
        request.extensions_mut().insert(ConnectInfo(peer));
        request
            .headers_mut()
            .insert("x-forwarded-for", HeaderValue::from_bytes(&[0xff]).unwrap());
        request
    };

    let (first, ..) = send(&harness.state, request()).await;
    let (second, ..) = send(&harness.state, request()).await;

    assert_eq!((first, second), (StatusCode::OK, StatusCode::TOO_MANY_REQUESTS));
}

#[tokio::test]
async fn test_trusted_proxy_canonicalizes_ipv4_mapped_addresses() {
    let harness = harness(
        limit_simple_with_trusted_proxies(1, &["198.51.100.0/24"]),
        DEFAULT_UPSTREAM_CONCURRENCY,
    )
    .await;
    let peer = "[::ffff:198.51.100.10]:5000".parse().unwrap();

    let first = status_with_peer(&harness.state, peer, &[("x-real-ip", "::ffff:192.0.2.1")]).await;
    let second = status_with_peer(&harness.state, peer, &[("x-real-ip", "192.0.2.2")]).await;
    let third = status_with_peer(&harness.state, peer, &[("x-real-ip", "192.0.2.1")]).await;

    assert_eq!(
        (first, second, third),
        (StatusCode::OK, StatusCode::OK, StatusCode::TOO_MANY_REQUESTS)
    );
}

#[rstest::rstest]
#[case::ipv4("198.51.100.10:5000", "198.51.100.0/24", "192.0.2.1", "192.0.2.2")]
#[case::ipv6("[2001:db8:1::10]:5000", "2001:db8:1::/64", "2001:db8:2::1", "2001:db8:2::2")]
#[tokio::test]
async fn test_trusted_proxy_accepts_real_ip_for_both_address_families(
    #[case] peer: &str,
    #[case] trusted_proxy: &str,
    #[case] first_client: &str,
    #[case] second_client: &str,
) {
    let harness = harness(
        limit_simple_with_trusted_proxies(1, &[trusted_proxy]),
        DEFAULT_UPSTREAM_CONCURRENCY,
    )
    .await;
    let peer = peer.parse().unwrap();

    let first = status_with_peer(&harness.state, peer, &[("x-real-ip", first_client)]).await;
    let second = status_with_peer(&harness.state, peer, &[("x-real-ip", second_client)]).await;

    assert_eq!((first, second), (StatusCode::OK, StatusCode::OK));
}

#[tokio::test]
async fn test_rotated_usernames_for_the_same_principal_share_a_bucket() {
    let statuses = principal_bucket_statuses([
        basic_authorization("pip", "secret"),
        basic_authorization("twine", "secret"),
    ])
    .await;

    assert_eq!(statuses, [StatusCode::OK, StatusCode::TOO_MANY_REQUESTS]);
}

#[tokio::test]
async fn test_distinct_principals_use_separate_buckets() {
    let statuses = principal_bucket_statuses([
        basic_authorization("pip", "secret"),
        basic_authorization("twine", "other-secret"),
    ])
    .await;

    assert_eq!(statuses, [StatusCode::OK, StatusCode::OK]);
}

async fn principal_bucket_statuses(credentials: [String; 2]) -> [StatusCode; 2] {
    let harness = authenticated_harness(limit_simple(1), DEFAULT_UPSTREAM_CONCURRENCY).await;
    let first_status = request(
        &harness.state,
        "/pypi/simple/",
        &[("x-forwarded-for", "192.0.2.1"), ("authorization", &credentials[0])],
    )
    .await
    .0;
    let second_status = request(
        &harness.state,
        "/pypi/simple/",
        &[("x-forwarded-for", "192.0.2.1"), ("authorization", &credentials[1])],
    )
    .await
    .0;

    [first_status, second_status]
}

fn basic_authorization(user: &str, password: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"))
    )
}

#[rstest::rstest]
#[case::basic("Basic invalid-one", "Basic invalid-two")]
#[case::bearer("Bearer invalid-one", "Bearer invalid-two")]
#[tokio::test]
async fn test_invalid_credentials_share_the_ip_bucket(#[case] first_credential: &str, #[case] second_credential: &str) {
    let harness = harness(limit_simple(1), DEFAULT_UPSTREAM_CONCURRENCY).await;

    let (first_status, ..) = request(
        &harness.state,
        "/pypi/simple/",
        &[("x-forwarded-for", "192.0.2.1"), ("authorization", first_credential)],
    )
    .await;
    let (second_status, ..) = request(
        &harness.state,
        "/pypi/simple/",
        &[("x-forwarded-for", "192.0.2.1"), ("authorization", second_credential)],
    )
    .await;

    assert_eq!(
        (first_status, second_status),
        (StatusCode::OK, StatusCode::TOO_MANY_REQUESTS)
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
    let harness = authenticated_harness(limit_simple(1), DEFAULT_UPSTREAM_CONCURRENCY).await;
    let headers = [
        ("x-forwarded-for", "192.0.2.1"),
        ("authorization", "Basic cGlwOnNlY3JldA=="),
    ];

    let _ = request(&harness.state, "/pypi/simple/", &headers).await;
    let _ = request(&harness.state, "/pypi/simple/", &headers).await;

    let events = capture.security_events();
    let event = events
        .iter()
        .find(|event| field(event, "client") == Some("token"))
        .unwrap_or_else(|| panic!("{}", capture.text()));
    assert_eq!(field(event, "event"), Some("rate_limit"));
    assert_eq!(field(event, "result"), Some("denied"));
}

async fn authenticated_harness(rate_limit: RateLimitConfig, upstream_concurrency: usize) -> Harness {
    harness_with_acl(
        rate_limit,
        upstream_concurrency,
        IndexAcl {
            anonymous_read: true,
            tokens: [("pip", "secret"), ("twine", "other-secret")]
                .into_iter()
                .map(|(name, secret)| NamedToken {
                    name: name.to_owned(),
                    secret: secret.to_owned(),
                    grants: Vec::new(),
                    expires_at: None,
                })
                .collect(),
        },
    )
    .await
}

async fn harness_with_acl(rate_limit: RateLimitConfig, upstream_concurrency: usize, acl: IndexAcl) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let state = super::wired(AppState::with_limits(
        MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        BlobStore::new(dir.path().join("blobs")),
        60,
        vec![Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Cached {
                client: UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap(),
                offline: false,
            },
            policy: Policy::default(),
            acl,
        }],
        Arc::new(|| 1000),
        rate_limit,
        [("pypi".to_owned(), upstream_concurrency)],
    ));
    Harness {
        _dir: dir,
        server,
        state,
    }
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
                acl: IndexAcl::default(),
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
                acl: IndexAcl::default(),
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
            acl: IndexAcl::default(),
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
    use peryx_driver::rate_limit::RouteClass;
    use peryx_driver::serving::EcosystemDriver as _;

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
