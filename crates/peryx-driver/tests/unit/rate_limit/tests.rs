use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, HeaderValue, Method, Request, StatusCode, Uri, header};
use axum::response::{IntoResponse as _, Response};
use axum::{Router, middleware, routing::get};
use peryx_core::Ecosystem;
use peryx_identity::IndexAcl;
use peryx_index::{Index, IndexKind};
use peryx_policy::Policy;
use peryx_search::default_indexer;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;
use tower::ServiceExt as _;

use super::{
    ActorKey, ForwardedClient, RateLimitConfig, RateLimiter, RouteClass, RouteLimit, UpstreamLimits, limited_response,
    malformed_forwarded_response, real_ip, service_route_class,
};
use crate::serving::{
    AbsoluteProtocolDriver, EcosystemDriver, IndexedProtocolDriver, ProtocolDriver, RateLimitPrincipal, ServiceDriver,
};
use crate::state::{AppState, ServingState};

struct IndexedDriver;

impl EcosystemDriver for IndexedDriver {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::new("example")
    }
}

impl RateLimitPrincipal for IndexedDriver {
    fn resolve(
        &self,
        _state: &crate::ServingState,
        _position: Option<usize>,
        _headers: &HeaderMap,
    ) -> peryx_identity::Principal {
        peryx_identity::Principal::Named {
            subject: "reader".to_owned(),
        }
    }
}

#[async_trait]
impl IndexedProtocolDriver for IndexedDriver {
    fn classify_route(&self, _path: &str) -> RouteClass {
        RouteClass::Artifact
    }

    async fn get(
        &self,
        _state: Arc<ServingState>,
        _position: usize,
        _rest: String,
        _uri: Uri,
        _headers: HeaderMap,
        _method: Method,
    ) -> Response {
        StatusCode::NO_CONTENT.into_response()
    }

    async fn post(&self, _state: Arc<ServingState>, _path: String, _request: axum::extract::Request) -> Response {
        StatusCode::NO_CONTENT.into_response()
    }

    async fn put(&self, _state: Arc<ServingState>, _request: axum::extract::Request) -> Response {
        StatusCode::NO_CONTENT.into_response()
    }

    async fn delete(&self, _state: Arc<ServingState>, _uri: Uri, _headers: HeaderMap) -> Response {
        StatusCode::NO_CONTENT.into_response()
    }
}

#[async_trait]
impl ServiceDriver for IndexedDriver {
    fn classify_service_post(&self, path: &str, _headers: &HeaderMap) -> Option<RouteClass> {
        (path == "+special").then_some(RouteClass::Admin)
    }

    async fn service_post(&self, _state: Arc<crate::ServingState>, _request: axum::extract::Request) -> Response {
        StatusCode::NO_CONTENT.into_response()
    }
}

struct AbsoluteDriver;

impl EcosystemDriver for AbsoluteDriver {
    fn ecosystem(&self) -> Ecosystem {
        Ecosystem::new("absolute")
    }
}

#[async_trait]
impl AbsoluteProtocolDriver for AbsoluteDriver {
    fn prefixes(&self) -> &'static [&'static str] {
        &["/artifacts"]
    }

    fn classify_route(&self, _path: &str) -> RouteClass {
        RouteClass::Artifact
    }

    async fn serve(&self, _state: Arc<ServingState>, _request: axum::extract::Request) -> Response {
        StatusCode::NO_CONTENT.into_response()
    }
}

#[tokio::test]
async fn test_protocol_fixtures_serve_supported_requests() {
    let (_dir, state) = app(RateLimitConfig::default());
    let serving = Arc::clone(&state.serving);
    let indexed = IndexedDriver;

    assert_eq!(
        indexed
            .get(
                Arc::clone(&serving),
                0,
                "resource".to_owned(),
                Uri::from_static("/items/resource"),
                HeaderMap::new(),
                Method::GET,
            )
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        indexed
            .post(
                Arc::clone(&serving),
                "/items".to_owned(),
                Request::builder().body(Body::from("post body")).unwrap(),
            )
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        indexed
            .put(
                Arc::clone(&serving),
                Request::builder()
                    .method(Method::PUT)
                    .uri("/items/resource")
                    .body(Body::from("put body"))
                    .unwrap(),
            )
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        indexed
            .delete(
                Arc::clone(&serving),
                Uri::from_static("/items/resource"),
                HeaderMap::new(),
            )
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        indexed
            .service_post(Arc::clone(&serving), Request::new(Body::empty()))
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        AbsoluteDriver
            .serve(serving, Request::get("/artifacts/item").body(Body::empty()).unwrap())
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
}

#[test]
fn test_check_client_allows_within_limit_then_denies_per_client() {
    let limiter = RateLimiter::new(RateLimitConfig {
        listing: RouteLimit::new(2, 60),
        ..RateLimitConfig::enabled_defaults()
    });
    let client = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7));
    assert!(limiter.check_client(RouteClass::Listing, client));
    assert!(limiter.check_client(RouteClass::Listing, client));
    assert!(!limiter.check_client(RouteClass::Listing, client));
    assert!(limiter.check_client(RouteClass::Listing, IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9))));
}

#[test]
fn test_the_window_resets_and_readmits_once_time_advances_past_it() {
    let millis = Arc::new(AtomicU64::new(0));
    let handle = Arc::clone(&millis);
    let limiter = RateLimiter::with_clock(
        RateLimitConfig {
            listing: RouteLimit::new(1, 1),
            ..RateLimitConfig::enabled_defaults()
        },
        Arc::new(move || Duration::from_millis(handle.load(Ordering::SeqCst))),
    );
    let client = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7));
    assert!(
        limiter.check_client(RouteClass::Listing, client),
        "the first request in the window is admitted"
    );
    assert!(
        !limiter.check_client(RouteClass::Listing, client),
        "the second exhausts the one-per-window budget"
    );

    millis.store(1_001, Ordering::SeqCst);
    assert!(
        limiter.check_client(RouteClass::Listing, client),
        "a window whose reset time has passed readmits the client"
    );
}

#[test]
fn test_service_route_class_handles_writes_and_service_routes() {
    assert_eq!(
        service_route_class(&Method::POST, "/alpha/items"),
        Some(RouteClass::Upload)
    );
    assert_eq!(service_route_class(&Method::GET, "/+status"), Some(RouteClass::Admin));
    assert_eq!(service_route_class(&Method::GET, "/+acl"), Some(RouteClass::Admin));
    assert_eq!(
        service_route_class(&Method::GET, "/+revocations"),
        Some(RouteClass::Admin)
    );
    assert_eq!(
        service_route_class(&Method::PUT, "/+revocations/sha256:digest"),
        Some(RouteClass::Admin)
    );
    assert_eq!(
        service_route_class(&Method::POST, "/+revocations/sha256:digest/lift"),
        Some(RouteClass::Admin)
    );
    assert_eq!(
        service_route_class(&Method::GET, "/alpha/hosted/+api"),
        Some(RouteClass::Admin)
    );
    assert_eq!(
        service_route_class(&Method::GET, "/alpha/resources/widget/details"),
        None
    );
}

#[test]
fn test_service_route_class_treats_head_and_options_as_reads() {
    assert_eq!(service_route_class(&Method::HEAD, "/service/resources/current"), None);
    assert_eq!(service_route_class(&Method::OPTIONS, "/alpha/items/current"), None);
    assert_eq!(service_route_class(&Method::HEAD, "/+status"), Some(RouteClass::Admin));
    for method in [Method::PUT, Method::PATCH, Method::DELETE] {
        assert_eq!(
            service_route_class(&method, "/service/resources/1"),
            Some(RouteClass::Upload)
        );
    }
    assert_eq!(
        service_route_class(&Method::TRACE, "/alpha/items"),
        Some(RouteClass::Upload)
    );
}

#[test]
fn test_route_classes_expose_stable_names_and_limits() {
    let config = RateLimitConfig::enabled_defaults();
    let expected = [
        (RouteClass::Listing, "listing", config.listing),
        (RouteClass::Metadata, "metadata", config.metadata),
        (RouteClass::Artifact, "artifact", config.artifact),
        (RouteClass::Upload, "upload", config.upload),
        (RouteClass::Admin, "admin", config.admin),
    ];

    assert_eq!(RouteClass::all(), expected.map(|(class, _, _)| class));
    for (class, name, limit) in expected {
        assert_eq!(class.as_str(), name);
        assert_eq!(config.limit(class), limit);
    }
}

#[test]
fn test_default_limiter_is_disabled_and_counts_each_class() {
    let limiter = RateLimiter::default();

    assert!(!limiter.enabled());
    for class in RouteClass::all() {
        assert!(limiter.check_client(class, IpAddr::V4(Ipv4Addr::LOCALHOST)));
    }
    assert_eq!(
        limiter
            .counters()
            .into_iter()
            .map(|snapshot| (snapshot.class, snapshot.allowed, snapshot.denied))
            .collect::<Vec<_>>(),
        vec![
            ("listing", 1, 0),
            ("metadata", 1, 0),
            ("artifact", 1, 0),
            ("upload", 1, 0),
            ("admin", 1, 0),
        ]
    );
}

#[test]
fn test_zero_limit_is_unbounded() {
    let limiter = RateLimiter::new(RateLimitConfig {
        listing: RouteLimit::new(0, 60),
        ..RateLimitConfig::enabled_defaults()
    });

    assert!(limiter.check_client(RouteClass::Listing, IpAddr::V4(Ipv4Addr::LOCALHOST)));
    assert!(limiter.check_client(RouteClass::Listing, IpAddr::V4(Ipv4Addr::LOCALHOST)));
}

#[test]
fn test_proxy_trust_canonicalizes_addresses() {
    let limiter = RateLimiter::new(RateLimitConfig {
        trusted_proxies: vec!["127.0.0.0/8".parse().unwrap()],
        ..RateLimitConfig::enabled_defaults()
    });

    assert!(limiter.trusts_proxy("::ffff:127.0.0.1".parse().unwrap()));
    assert!(!limiter.trusts_proxy("198.51.100.1".parse().unwrap()));
}

#[test]
fn test_actor_key_uses_subject_or_resolved_client() {
    let limiter = RateLimiter::default();
    let request = Request::new(Body::empty());

    assert!(matches!(
        limiter
            .actor_key(
                peryx_identity::Principal::Named {
                    subject: "user".to_owned(),
                },
                &request,
            )
            .unwrap(),
        ActorKey::Token(_)
    ));
    assert_eq!(
        limiter
            .actor_key(peryx_identity::Principal::Anonymous, &request)
            .unwrap(),
        ActorKey::Ip(IpAddr::V4(Ipv4Addr::LOCALHOST))
    );
}

fn proxied_limiter() -> RateLimiter {
    RateLimiter::new(RateLimitConfig {
        trusted_proxies: vec!["10.0.0.0/8".parse().unwrap()],
        ..RateLimitConfig::enabled_defaults()
    })
}

fn proxied_request() -> Request<Body> {
    let mut request = Request::new(Body::empty());
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([10, 0, 0, 1], 8080))));
    request
}

#[test]
fn test_client_ip_ignores_forwarded_headers_from_untrusted_peer() {
    let limiter = proxied_limiter();
    let mut request = Request::new(Body::empty());
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([192, 0, 2, 1], 8080))));
    request
        .headers_mut()
        .insert("x-forwarded-for", HeaderValue::from_static("198.51.100.1"));

    assert_eq!(
        limiter.client_ip(&request).unwrap(),
        Some(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)))
    );
}

#[test]
fn test_forwarded_chain_uses_rightmost_untrusted_client() {
    let limiter = proxied_limiter();
    let mut headers = HeaderMap::new();
    headers.append("x-forwarded-for", HeaderValue::from_static("192.0.2.1, 10.0.0.2"));
    headers.append("x-forwarded-for", HeaderValue::from_static("198.51.100.2"));

    assert!(matches!(
        limiter.forwarded_client_ip(&headers),
        ForwardedClient::Resolved(IpAddr::V4(address)) if address == Ipv4Addr::new(198, 51, 100, 2)
    ));
}

#[test]
fn test_forwarded_chain_rejects_malformed_suffix() {
    let limiter = proxied_limiter();
    let mut headers = HeaderMap::new();
    headers.insert("x-forwarded-for", HeaderValue::from_static("192.0.2.1, malformed"));

    assert!(matches!(
        limiter.forwarded_client_ip(&headers),
        ForwardedClient::Malformed
    ));
}

#[test]
fn test_fully_trusted_chain_uses_peer() {
    let limiter = proxied_limiter();
    let mut request = proxied_request();
    request
        .headers_mut()
        .insert("x-forwarded-for", HeaderValue::from_static("10.0.0.2"));

    assert_eq!(
        limiter.client_ip(&request).unwrap(),
        Some(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)))
    );
}

#[test]
fn test_real_ip_requires_one_valid_address() {
    assert!(matches!(real_ip(&HeaderMap::new()), ForwardedClient::TrustedChain));

    let mut headers = HeaderMap::new();
    headers.insert("x-real-ip", HeaderValue::from_static("198.51.100.2"));
    assert!(matches!(real_ip(&headers), ForwardedClient::Resolved(_)));

    headers.append("x-real-ip", HeaderValue::from_static("198.51.100.3"));
    assert!(matches!(real_ip(&headers), ForwardedClient::Malformed));

    let mut invalid = HeaderMap::new();
    invalid.insert("x-real-ip", HeaderValue::from_bytes(b"\xff").unwrap());
    assert!(matches!(real_ip(&invalid), ForwardedClient::Malformed));
}

#[tokio::test]
async fn test_upstream_limits_handle_unconfigured_unbounded_and_bounded_indexes() {
    let limits = UpstreamLimits::new([("unbounded".to_owned(), 0), ("bounded".to_owned(), 1)]);

    assert!(limits.acquire("missing").await.unwrap().is_none());
    assert!(limits.acquire("unbounded").await.unwrap().is_none());
    let permit = limits.acquire("bounded").await.unwrap().unwrap();
    assert_eq!(
        limits
            .snapshots()
            .into_iter()
            .map(|snapshot| (
                snapshot.index,
                snapshot.max_concurrent,
                snapshot.in_flight,
                snapshot.denied
            ))
            .collect::<Vec<_>>(),
        vec![("bounded".to_owned(), 1, 1, 0), ("unbounded".to_owned(), 0, 0, 0)]
    );
    assert_eq!(limits.totals().in_flight, 1);
    drop(permit);
    assert_eq!(limits.totals().in_flight, 0);
}

#[tokio::test(start_paused = true)]
async fn test_upstream_limit_times_out_with_retry_horizon() {
    let limits = Arc::new(UpstreamLimits::new([("bounded".to_owned(), 1)]));
    let _permit = limits.acquire("bounded").await.unwrap().unwrap();
    let waiting_limits = Arc::clone(&limits);
    let waiting = tokio::spawn(async move { waiting_limits.acquire("bounded").await });
    tokio::time::advance(Duration::from_secs(30)).await;

    let error = waiting.await.unwrap().unwrap_err();

    assert_eq!(error.retry_after, 30);
    assert_eq!(limits.snapshots()[0].denied, 1);
    assert_eq!(limits.totals().denied, 1);
}

fn app(config: RateLimitConfig) -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    (dir, AppState::with_rate_limits(meta, blobs, 60, Vec::new(), config, []))
}

fn router(state: AppState) -> Router {
    Router::new()
        .fallback(get(|| async { StatusCode::NO_CONTENT }))
        .layer(middleware::from_fn_with_state(Arc::new(state), super::enforce))
}

#[tokio::test]
async fn test_enforce_bypasses_health_and_limits_admin_requests() {
    let config = RateLimitConfig {
        admin: RouteLimit::new(1, 60),
        ..RateLimitConfig::enabled_defaults()
    };
    let (_dir, state) = app(config);
    let router = router(state);

    assert_eq!(
        router
            .clone()
            .oneshot(Request::get("/+health").body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        router
            .clone()
            .oneshot(Request::get("/+status").body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status(),
        StatusCode::NO_CONTENT
    );
    let response = router
        .oneshot(Request::get("/+status").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(
        (1..=60).contains(
            &response.headers()[header::RETRY_AFTER]
                .to_str()
                .unwrap()
                .parse::<u64>()
                .unwrap()
        )
    );
}

#[tokio::test]
async fn test_enforce_rejects_malformed_forwarded_identity() {
    let config = RateLimitConfig {
        trusted_proxies: vec!["10.0.0.0/8".parse().unwrap()],
        ..RateLimitConfig::enabled_defaults()
    };
    let (_dir, state) = app(config);
    let mut request = Request::get("/+status").body(Body::empty()).unwrap();
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([10, 0, 0, 1], 8080))));
    request
        .headers_mut()
        .insert("x-forwarded-for", HeaderValue::from_static("malformed"));

    assert_eq!(
        router(state).oneshot(request).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn test_enforce_uses_the_indexed_drivers_route_class() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::with_rate_limits(
        meta,
        blobs,
        60,
        vec![Index {
            name: "items".to_owned(),
            route: "items".to_owned(),
            ecosystem: Ecosystem::new("example"),
            kind: IndexKind::Hosted { volatile: true },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        }],
        RateLimitConfig {
            artifact: RouteLimit::new(1, 60),
            ..RateLimitConfig::enabled_defaults()
        },
        [],
    );
    state.register_rate_limit_principal(Ecosystem::new("example"), &IndexedDriver);
    state
        .register_protocol(ProtocolDriver::Indexed(Arc::new(IndexedDriver)), default_indexer())
        .unwrap();
    let router = router(state);

    let request = || {
        Request::get("/items/resource")
            .header(header::AUTHORIZATION, "opaque")
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        router.clone().oneshot(request()).await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        router.oneshot(request()).await.unwrap().status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn test_enforce_uses_an_ecosystem_service_post_class() {
    let config = RateLimitConfig {
        admin: RouteLimit::new(1, 60),
        ..RateLimitConfig::enabled_defaults()
    };
    let (_dir, mut state) = app(config);
    state.register_capabilities(|registrar| {
        registrar.register_service(Ecosystem::new("example"), Arc::new(IndexedDriver));
    });
    let router = router(state);
    let request = || Request::post("/+special").body(Body::empty()).unwrap();

    assert_eq!(
        router.clone().oneshot(request()).await.unwrap().status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        router.oneshot(request()).await.unwrap().status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn test_enforce_uses_an_absolute_drivers_route_class() {
    let config = RateLimitConfig {
        artifact: RouteLimit::new(1, 60),
        ..RateLimitConfig::enabled_defaults()
    };
    let (_dir, mut state) = app(config);
    state
        .register_protocol(ProtocolDriver::Absolute(Arc::new(AbsoluteDriver)), default_indexer())
        .unwrap();
    let router = router(state);
    let request = || Request::get("/artifacts/item").body(Body::empty()).unwrap();

    assert_eq!(
        router.clone().oneshot(request()).await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        router.oneshot(request()).await.unwrap().status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn test_enforce_falls_back_to_the_listing_class() {
    let config = RateLimitConfig {
        listing: RouteLimit::new(1, 60),
        ..RateLimitConfig::enabled_defaults()
    };
    let (_dir, state) = app(config);
    let router = router(state);
    let request = || Request::get("/unknown").body(Body::empty()).unwrap();

    assert_eq!(
        router.clone().oneshot(request()).await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        router.oneshot(request()).await.unwrap().status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn test_enforce_skips_drivers_without_service_posts() {
    let config = RateLimitConfig {
        upload: RouteLimit::new(1, 60),
        ..RateLimitConfig::enabled_defaults()
    };
    let (_dir, mut state) = app(config);
    state.register_driver(Arc::new(AbsoluteDriver));
    let router = router(state);
    let request = || Request::post("/upload").body(Body::empty()).unwrap();

    assert_eq!(
        router.clone().oneshot(request()).await.unwrap().status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        router.oneshot(request()).await.unwrap().status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn test_enforce_falls_back_when_service_posts_decline() {
    let config = RateLimitConfig {
        upload: RouteLimit::new(1, 60),
        ..RateLimitConfig::enabled_defaults()
    };
    let (_dir, mut state) = app(config);
    state.register_capabilities(|registrar| {
        registrar.register_service(Ecosystem::new("example"), Arc::new(IndexedDriver));
    });
    let router = router(state);
    let request = || Request::post("/upload").body(Body::empty()).unwrap();

    assert_eq!(
        router.clone().oneshot(request()).await.unwrap().status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        router.oneshot(request()).await.unwrap().status(),
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[test]
fn test_client_ip_resolves_forwarded_and_real_ip_headers() {
    let limiter = proxied_limiter();
    let mut forwarded = proxied_request();
    forwarded
        .headers_mut()
        .insert("x-forwarded-for", HeaderValue::from_static("198.51.100.1"));
    let mut real = proxied_request();
    real.headers_mut()
        .insert("x-real-ip", HeaderValue::from_static("203.0.113.2"));

    assert_eq!(
        limiter.client_ip(&forwarded).unwrap(),
        Some("198.51.100.1".parse().unwrap())
    );
    assert_eq!(limiter.client_ip(&real).unwrap(), Some("203.0.113.2".parse().unwrap()));
}

#[test]
fn test_forwarded_chain_rejects_non_utf8_header_values() {
    let limiter = proxied_limiter();
    let mut headers = HeaderMap::new();
    headers.insert("x-forwarded-for", HeaderValue::from_bytes(&[0xff]).unwrap());

    assert!(matches!(
        limiter.forwarded_client_ip(&headers),
        ForwardedClient::Malformed
    ));
}

#[test]
fn test_rate_limit_errors_keep_status_and_retry_header() {
    assert_eq!(malformed_forwarded_response().status(), StatusCode::BAD_REQUEST);
    let response = limited_response(41);
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(response.headers()[header::RETRY_AFTER], "41");
}
