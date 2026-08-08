//! The serving registry: what a state does before any ecosystem driver is wired in, and how it
//! keeps several route-mounted ecosystems apart once they are.

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode};
use peryx_driver::rate_limit::RateLimitConfig;
use peryx_identity::IndexAcl;
use rstest::rstest;
use tower::ServiceExt as _;

use peryx_driver::state::{AppState, ServingState};

fn unwired_state() -> (tempfile::TempDir, std::sync::Arc<AppState>) {
    unwired_state_with(Vec::new())
}

fn unwired_state_with(indexes: Vec<peryx_driver::state::Index>) -> (tempfile::TempDir, std::sync::Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    (dir, std::sync::Arc::new(AppState::new(meta, blobs, 60, indexes)))
}

fn unwired_state_with_limits(rate_limit: RateLimitConfig) -> (tempfile::TempDir, std::sync::Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    (
        dir,
        std::sync::Arc::new(AppState::with_rate_limits(
            meta,
            blobs,
            60,
            Vec::new(),
            rate_limit,
            std::iter::empty(),
        )),
    )
}

fn test_index(route: &str) -> peryx_driver::state::Index {
    peryx_driver::state::Index {
        name: route.to_owned(),
        route: route.to_owned(),
        ecosystem: peryx_core::Ecosystem::new("example"),
        kind: peryx_driver::state::IndexKind::Hosted { volatile: false },
        policy: peryx_policy::Policy::default(),
        acl: IndexAcl::default(),
    }
}

#[rstest]
#[case::liveness("/+health", r#"{"status":"live"}"#)]
#[case::readiness("/+ready", r#"{"status":"ready"}"#)]
#[tokio::test]
async fn test_unwired_state_serves_public_probes(#[case] uri: &str, #[case] expected: &str) {
    let (_dir, state) = unwired_state_with(vec![test_index("private-route")]);
    let response = crate::router(state)
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[axum::http::header::CACHE_CONTROL], "no-store");
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.as_ref(), expected.as_bytes());
}

#[tokio::test]
async fn test_unwired_state_serves_503_when_a_driver_is_missing() {
    // A configured index with no ecosystem driver wired in: resolvable, so the request reaches the
    // driver seam and fails loudly rather than serving nothing.
    let (_dir, state) = unwired_state_with(vec![test_index("alpha")]);
    let app = crate::router(state);
    let cases = [
        (Method::GET, "/alpha/simple/", Body::empty(), None),
        (Method::PUT, "/alpha/flask/1.0/yank", Body::empty(), None),
        (Method::DELETE, "/alpha/flask/1.0/", Body::empty(), None),
        (
            Method::POST,
            "/alpha/",
            Body::from("--x--\r\n"),
            Some("multipart/form-data; boundary=x"),
        ),
    ];
    for (method, uri, body, content_type) in cases {
        let mut builder = Request::builder().method(method.clone()).uri(uri);
        if let Some(content_type) = content_type {
            builder = builder.header("content-type", content_type);
        }
        let response = app.clone().oneshot(builder.body(body).unwrap()).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "{method} {uri} should be 503 without a driver",
        );
    }
}

#[tokio::test]
async fn test_get_for_an_unknown_route_is_not_found() {
    // The neutral GET dispatch resolves the index before touching a driver, so a path under no
    // configured route is a plain 404.
    let (_dir, state) = unwired_state();
    let app = crate::router(state);
    let response = app
        .oneshot(Request::builder().uri("/nope/simple/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_unwired_state_discovery_lists_no_indexes() {
    // `/+api` is a neutral service endpoint: it describes the running server and needs no ecosystem
    // driver, so an unwired state answers `200` with an empty index list rather than `503`.
    let (_dir, state) = unwired_state();
    let app = crate::router(state);
    let response = app
        .oneshot(Request::builder().uri("/+api").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let document: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(document["indexes"].as_array().unwrap().is_empty());
    assert!(document["urls"]["status"].is_string());
}

#[rstest]
#[case::missing(None, "http://internal.test/+status")]
#[case::untrusted(Some("192.0.2.1:443"), "http://internal.test/+status")]
#[case::trusted(Some("127.0.0.1:443"), "https://packages.example/+status")]
#[tokio::test]
async fn test_discovery_accepts_forwarded_origin_only_from_a_trusted_proxy(
    #[case] peer: Option<&str>,
    #[case] expected: &str,
) {
    let (_dir, state) = unwired_state_with_limits(RateLimitConfig {
        trusted_proxies: vec!["127.0.0.1/32".parse().unwrap()],
        ..RateLimitConfig::default()
    });
    let app = crate::router(state);
    let mut request = Request::builder()
        .uri("/+api")
        .header("host", "internal.test")
        .header("x-forwarded-host", "packages.example")
        .header("x-forwarded-proto", "https")
        .body(Body::empty())
        .unwrap();
    if let Some(peer) = peer {
        request
            .extensions_mut()
            .insert(ConnectInfo(peer.parse::<std::net::SocketAddr>().unwrap()));
    }
    let response = app.oneshot(request).await.unwrap();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let document: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(document["urls"]["status"], expected);
}

#[tokio::test]
async fn test_unwired_discovery_renders_a_minimal_entry_per_index() {
    use peryx_core::Ecosystem;

    use peryx_driver::state::{Index, IndexKind};

    let dir = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    let index = Index {
        name: "alpha".to_owned(),
        route: "alpha".to_owned(),
        ecosystem: Ecosystem::new("example"),
        kind: IndexKind::Hosted { volatile: false },
        policy: peryx_policy::Policy::default(),
        acl: IndexAcl::default(),
    };
    // Without an ecosystem driver, an index still appears in discovery through the neutral fallback:
    // its identity, but none of the wire-protocol URLs a real driver would render.
    let state = std::sync::Arc::new(AppState::new(meta, blobs, 60, vec![index]));
    let app = crate::router(state);
    let response = app
        .oneshot(Request::builder().uri("/+api").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let document: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let entry = &document["indexes"][0];
    assert_eq!(entry["route"], "alpha");
    assert_eq!(entry["ecosystem"], "example");
    assert_eq!(entry["urls"], serde_json::Value::Null);
}

/// A driver that answers with its own ecosystem's name, so a test can tell which one served.
struct StubServing(peryx_core::Ecosystem);

#[async_trait::async_trait]
impl peryx_driver::serving::EcosystemDriver for StubServing {
    fn ecosystem(&self) -> peryx_core::Ecosystem {
        self.0
    }

    async fn get(
        &self,
        _state: std::sync::Arc<ServingState>,
        _position: usize,
        _rest: String,
        _uri: axum::http::Uri,
        _headers: axum::http::HeaderMap,
        _method: axum::http::Method,
    ) -> axum::response::Response {
        axum::response::IntoResponse::into_response(self.0.as_str().to_owned())
    }

    async fn post(
        &self,
        _state: std::sync::Arc<ServingState>,
        _path: String,
        _headers: axum::http::HeaderMap,
        _multipart: axum::extract::Multipart,
    ) -> axum::response::Response {
        axum::response::IntoResponse::into_response(StatusCode::OK)
    }

    async fn put(
        &self,
        _state: std::sync::Arc<ServingState>,
        _uri: axum::http::Uri,
        _headers: axum::http::HeaderMap,
    ) -> axum::response::Response {
        axum::response::IntoResponse::into_response(StatusCode::OK)
    }

    async fn delete(
        &self,
        _state: std::sync::Arc<ServingState>,
        _uri: axum::http::Uri,
        _headers: axum::http::HeaderMap,
    ) -> axum::response::Response {
        axum::response::IntoResponse::into_response(StatusCode::OK)
    }

    fn discover_index(
        &self,
        index: peryx_driver::state::IndexDescription,
        _base: Option<&peryx_driver::discovery::BaseUrl>,
    ) -> serde_json::Value {
        peryx_driver::discovery::minimal_entry(&index)
    }

    fn classify_route(&self, _path: &str) -> peryx_driver::rate_limit::RouteClass {
        peryx_driver::rate_limit::RouteClass::Listing
    }
}

#[test]
fn test_a_driver_resolves_no_rate_limit_principal_by_default() {
    use peryx_driver::serving::EcosystemDriver as _;

    let (_dir, state) = unwired_state();
    assert_eq!(
        StubServing(peryx_core::Ecosystem::new("example")).rate_limit_principal(
            &state,
            None,
            &axum::http::HeaderMap::new()
        ),
        peryx_identity::Principal::Anonymous
    );
}

#[test]
fn test_an_unwired_state_holds_no_driver_for_any_ecosystem() {
    let (_dir, state) = unwired_state();
    assert!(!state.has_any_driver());
    for ecosystem in [
        peryx_core::Ecosystem::new("example"),
        peryx_core::Ecosystem::new("other"),
    ] {
        assert!(state.driver_for(ecosystem).is_none(), "{ecosystem} was wired in");
    }
}

#[tokio::test]
async fn test_post_without_multipart_content_type_is_bad_request() {
    let dir = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::new(meta, blobs, 60, vec![test_index("alpha")]);
    state.register_ecosystem(
        std::sync::Arc::new(StubServing(peryx_core::Ecosystem::new("example"))),
        std::sync::Arc::new(peryx_search::EmptyIndexer),
    );

    let response = crate::router(std::sync::Arc::new(state))
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/alpha/")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_two_route_mounted_ecosystems_each_serve_their_own_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    let mut other = test_index("images");
    other.ecosystem = peryx_core::Ecosystem::new("other");
    let mut state = AppState::new(meta, blobs, 60, vec![test_index("alpha"), other]);
    // Registering a second driver must not displace the first: each keeps its own slot.
    state.register_ecosystem(
        std::sync::Arc::new(StubServing(peryx_core::Ecosystem::new("example"))),
        std::sync::Arc::new(peryx_search::EmptyIndexer),
    );
    state.register_ecosystem(
        std::sync::Arc::new(StubServing(peryx_core::Ecosystem::new("other"))),
        std::sync::Arc::new(peryx_search::EmptyIndexer),
    );
    let app = crate::router(std::sync::Arc::new(state));

    for (route, expected) in [("alpha", "example"), ("images", "other")] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/{route}/anything"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body, expected.as_bytes(), "/{route} was served by the wrong driver");
    }
}

/// A driver implementing only the three required methods, so every other method takes its trait
/// default. It exercises the neutral defaults an ecosystem inherits when its format has no concept for
/// them: an empty browse view, an unsupported operation, the wrong-mount guard.
struct BareDriver;

#[async_trait::async_trait]
impl peryx_driver::serving::EcosystemDriver for BareDriver {
    fn ecosystem(&self) -> peryx_core::Ecosystem {
        peryx_core::Ecosystem::new("example")
    }

    fn classify_route(&self, _path: &str) -> peryx_driver::rate_limit::RouteClass {
        peryx_driver::rate_limit::RouteClass::Listing
    }

    fn discover_index(
        &self,
        index: peryx_driver::state::IndexDescription,
        _base: Option<&peryx_driver::discovery::BaseUrl>,
    ) -> serde_json::Value {
        peryx_driver::discovery::minimal_entry(&index)
    }
}

#[tokio::test]
async fn test_bare_driver_serving_methods_reach_the_wrong_mount_guard() {
    use axum::extract::{FromRequest as _, Multipart, Request};
    use axum::http::{HeaderMap, Uri};
    use peryx_driver::serving::EcosystemDriver as _;

    // A driver's mount serves one half of the method set; the unused half's default is the loud guard,
    // so every one of these answers 500 rather than silently serving nothing.
    let (_dir, state) = unwired_state();
    let driver = BareDriver;
    let serving = state.serving.clone();
    let serve = driver
        .serve(serving.clone(), Request::builder().body(Body::empty()).unwrap())
        .await;
    assert_eq!(serve.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let get = driver
        .get(
            serving.clone(),
            0,
            "rest".to_owned(),
            Uri::from_static("/x"),
            HeaderMap::new(),
            Method::GET,
        )
        .await;
    assert_eq!(get.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let multipart = Multipart::from_request(
        Request::builder()
            .header("content-type", "multipart/form-data; boundary=x")
            .body(Body::from("--x--\r\n"))
            .unwrap(),
        &(),
    )
    .await
    .unwrap();
    let post = driver
        .post(serving.clone(), "x".to_owned(), HeaderMap::new(), multipart)
        .await;
    assert_eq!(post.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let put = driver
        .put(serving.clone(), Uri::from_static("/x"), HeaderMap::new())
        .await;
    assert_eq!(put.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let delete = driver
        .delete(serving.clone(), Uri::from_static("/x"), HeaderMap::new())
        .await;
    assert_eq!(delete.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[test]
fn test_bare_driver_neutral_defaults() {
    use peryx_driver::serving::EcosystemDriver as _;

    let driver = BareDriver;
    assert_eq!(driver.client_endpoint("my-index"), "/my-index/");
    let capabilities = driver.capabilities();
    assert!(capabilities.jobs.is_none());
    assert!(capabilities.metrics.is_none());
    assert!(capabilities.policy.is_none());
    assert!(capabilities.blob_references.is_none());
    assert!(capabilities.fsck.is_none());
    assert!(capabilities.retention.is_none());
    assert!(capabilities.cache.is_none());
    assert!(capabilities.index_summary.is_none());
    assert!(capabilities.trash.is_none());
    assert!(capabilities.shadow.is_none());
    assert!(capabilities.import.is_none());
    assert!(capabilities.service.is_none());
    assert!(capabilities.browse.is_none());
    assert!(capabilities.project_page.is_none());
    assert!(capabilities.manifest.is_none());
    assert!(capabilities.artifact_members.is_none());
    assert!(capabilities.artifact_path.is_none());
    assert!(capabilities.archive.is_none());
    assert!(capabilities.upload_ui.is_none());
}

#[tokio::test]
async fn test_unwired_state_search_returns_empty() {
    let (_dir, state) = unwired_state();
    let app = crate::router(state);
    let response = app
        .oneshot(Request::builder().uri("/+search?q=flask").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let document: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(document["total"], 0);
    assert!(document["results"].as_array().unwrap().is_empty());
}
