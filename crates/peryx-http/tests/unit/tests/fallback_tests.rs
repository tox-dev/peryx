//! Serving before driver registration and with several implementations installed.

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::response::{IntoResponse as _, Response};
use peryx_driver::http_services::HttpDomainServices;
use peryx_driver::rate_limit::{RateLimitConfig, RouteLimit};
use peryx_identity::IndexAcl;
use rstest::rstest;
use tower::ServiceExt as _;

use peryx_driver::serving::{
    AbsoluteProtocolDriver, EcosystemDriver as _, IndexedProtocolDriver, ProtocolDriver, ServiceDriver,
};
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

fn state_with_search_storage_failure(
    indexes: Vec<peryx_driver::state::Index>,
) -> (tempfile::TempDir, std::sync::Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    drop(peryx_storage::meta::MetaStore::open(&path).unwrap());
    let database = redb::Database::open(&path).unwrap();
    let transaction = database.begin_write().unwrap();
    transaction
        .delete_table(redb::TableDefinition::<&str, u64>::new("serial"))
        .unwrap();
    transaction
        .open_table(redb::TableDefinition::<&str, &[u8]>::new("serial"))
        .unwrap();
    transaction.commit().unwrap();
    drop(database);
    let meta = peryx_storage::meta::MetaStore::open_existing(path).unwrap();
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
    let (_dir, state) = unwired_state_with(vec![test_index("alpha")]);
    let app = crate::router(state);
    let cases = [
        (Method::GET, "/alpha/catalog/", Body::empty(), None),
        (Method::PUT, "/alpha/artifacts/item", Body::empty(), None),
        (Method::DELETE, "/alpha/artifacts/item", Body::empty(), None),
        (Method::POST, "/alpha/", Body::from("{}"), Some("application/json")),
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
        assert!(!response.headers().contains_key(axum::http::header::RETRY_AFTER));
    }
}

#[tokio::test]
async fn test_get_for_an_unknown_route_is_not_found() {
    let (_dir, state) = unwired_state();
    let app = crate::router(state);
    let response = app
        .oneshot(Request::builder().uri("/nope/catalog/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_unwired_state_discovery_lists_no_indexes() {
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

#[tokio::test]
async fn test_openapi_document_is_served() {
    let (_dir, state) = unwired_state();
    let response = crate::router(state)
        .oneshot(
            Request::builder()
                .uri("/api-docs/openapi.json")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[axum::http::header::CONTENT_TYPE], "application/json");
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

#[tokio::test]
async fn test_discovery_uses_the_registered_ecosystem_renderer() {
    let (_dir, state) = unwired_state_with(vec![test_index("alpha")]);
    let mut state = std::sync::Arc::into_inner(state).unwrap();
    super::support::register_example_driver(&mut state);

    let response = crate::router(std::sync::Arc::new(state))
        .oneshot(Request::builder().uri("/+api").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let document: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(document["indexes"][0]["route"], "alpha");
}

/// A driver that answers with its own ecosystem's name, so a test can tell which one served.
struct StubServing(peryx_core::Ecosystem);

impl peryx_driver::serving::EcosystemDriver for StubServing {
    fn ecosystem(&self) -> peryx_core::Ecosystem {
        self.0.clone()
    }
}

#[async_trait::async_trait]
impl IndexedProtocolDriver for StubServing {
    fn classify_route(&self, _path: &str) -> peryx_driver::rate_limit::RouteClass {
        peryx_driver::rate_limit::RouteClass::Listing
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
        request: axum::extract::Request,
    ) -> axum::response::Response {
        request_summary(request).await
    }

    async fn put(&self, _state: std::sync::Arc<ServingState>, request: axum::extract::Request) -> Response {
        request_summary(request).await
    }

    async fn delete(
        &self,
        _state: std::sync::Arc<ServingState>,
        _uri: axum::http::Uri,
        _headers: axum::http::HeaderMap,
    ) -> axum::response::Response {
        axum::response::IntoResponse::into_response(StatusCode::OK)
    }
}

async fn request_summary(request: axum::extract::Request) -> Response {
    let (parts, body) = request.into_parts();
    let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    format!(
        "{} {} {} {}",
        parts.method,
        parts.uri,
        parts.headers[axum::http::header::CONTENT_TYPE].to_str().unwrap(),
        String::from_utf8(body.to_vec()).unwrap(),
    )
    .into_response()
}

struct AbsoluteDriver;

impl peryx_driver::serving::EcosystemDriver for AbsoluteDriver {
    fn ecosystem(&self) -> peryx_core::Ecosystem {
        peryx_core::Ecosystem::new("absolute")
    }
}

#[async_trait::async_trait]
impl AbsoluteProtocolDriver for AbsoluteDriver {
    fn prefixes(&self) -> &'static [&'static str] {
        &["/artifacts"]
    }

    fn classify_route(&self, _path: &str) -> peryx_driver::rate_limit::RouteClass {
        peryx_driver::rate_limit::RouteClass::Artifact
    }

    async fn serve(&self, _state: std::sync::Arc<ServingState>, request: axum::extract::Request) -> Response {
        request.uri().path().to_owned().into_response()
    }
}

struct ServiceStub;

#[async_trait::async_trait]
impl ServiceDriver for ServiceStub {
    fn classify_service_post(&self, path: &str, _headers: &HeaderMap) -> Option<peryx_driver::rate_limit::RouteClass> {
        (path == "service/read").then_some(peryx_driver::rate_limit::RouteClass::Listing)
    }

    async fn service_post(&self, _state: std::sync::Arc<ServingState>, _request: axum::extract::Request) -> Response {
        StatusCode::NO_CONTENT.into_response()
    }
}

#[rstest]
#[case::root("/artifacts")]
#[case::nested("/artifacts/layers/sha256:a")]
#[tokio::test]
async fn test_absolute_mounts_own_exact_and_nested_paths(#[case] uri: &str) {
    let (_dir, mut state) = {
        let (dir, state) = unwired_state();
        (dir, std::sync::Arc::into_inner(state).unwrap())
    };
    register_absolute(&mut state, AbsoluteDriver);
    let response = crate::router(std::sync::Arc::new(state))
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        uri
    );
}

#[tokio::test]
async fn test_absolute_protocol_is_not_routable_through_an_index() {
    let mut index = test_index("alpha");
    index.ecosystem = peryx_core::Ecosystem::new("absolute");
    let (_dir, state) = unwired_state_with(vec![index]);
    let mut state = std::sync::Arc::into_inner(state).unwrap();
    register_absolute(&mut state, AbsoluteDriver);

    let response = crate::router(std::sync::Arc::new(state))
        .oneshot(Request::builder().uri("/alpha/item").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[rstest]
#[case::writable(false)]
#[case::read_only(true)]
#[tokio::test]
async fn test_service_reads_dispatch_on_writable_and_read_only_nodes(#[case] read_only: bool) {
    let (dir, state) = unwired_state_with(vec![test_index("alpha")]);
    let mut state = std::sync::Arc::into_inner(state).unwrap();
    state.set_read_only(read_only).unwrap();
    state.register_capabilities(|registrar| {
        registrar.register_service(peryx_core::Ecosystem::new("example"), std::sync::Arc::new(ServiceStub));
    });
    let response = crate::router(std::sync::Arc::new(state))
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/service/read")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    drop(dir);
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn test_read_only_node_rejects_unclassified_service_posts() {
    let (_dir, state) = unwired_state_with(vec![test_index("alpha")]);
    let mut state = std::sync::Arc::into_inner(state).unwrap();
    state.set_read_only(true).unwrap();
    state.register_capabilities(|registrar| {
        registrar.register_service(peryx_core::Ecosystem::new("example"), std::sync::Arc::new(ServiceStub));
    });
    let response = crate::router(std::sync::Arc::new(state))
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/service/write")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(!response.headers().contains_key(axum::http::header::RETRY_AFTER));
}

#[tokio::test]
async fn test_replica_retry_interval_rounds_up_to_delta_seconds() {
    let (_dir, state) = unwired_state_with(vec![test_index("alpha")]);
    let mut state = std::sync::Arc::into_inner(state).unwrap();
    state.set_read_only(true).unwrap();
    state
        .set_read_only_retry_after(Some(std::time::Duration::from_millis(1501)))
        .unwrap();

    let response = crate::router(std::sync::Arc::new(state))
        .oneshot(
            Request::builder()
                .method(Method::DELETE)
                .uri("/+grants/alpha")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers()[axum::http::header::RETRY_AFTER], "2");
}

#[tokio::test]
async fn test_read_only_node_preserves_method_not_allowed_for_process_routes() {
    let (_dir, state) = unwired_state_with_limits(RateLimitConfig::enabled_defaults());
    let mut state = std::sync::Arc::into_inner(state).unwrap();
    state.set_read_only(true).unwrap();

    let response = crate::router(std::sync::Arc::new(state))
        .oneshot(
            Request::builder()
                .method(Method::TRACE)
                .uri("/+repositories")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn test_rate_limit_layer_is_mounted_when_enabled() {
    let (_dir, state) = unwired_state_with_limits(RateLimitConfig {
        admin: RouteLimit::new(1, 60),
        ..RateLimitConfig::enabled_defaults()
    });
    let app = crate::router(state);
    assert_eq!(
        request_statuses(&app, Method::GET, "/+health").await,
        [StatusCode::OK; 2]
    );
}

#[rstest]
#[case::read(Method::GET)]
#[case::write(Method::POST)]
#[tokio::test]
async fn test_management_routes_use_the_admin_limit(#[case] method: Method) {
    let (_dir, state) = unwired_state_with_limits(RateLimitConfig {
        admin: RouteLimit::new(1, 60),
        ..RateLimitConfig::enabled_defaults()
    });

    let app = crate::router(state);
    let management = request_statuses(&app, method, "/+repositories").await;
    let package_read = app
        .oneshot(Request::get("/unknown").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(
        (management[1], package_read.status()),
        (StatusCode::TOO_MANY_REQUESTS, StatusCode::NOT_FOUND)
    );
}

#[rstest]
#[case::read(Method::GET, "/_/session")]
#[case::write(Method::POST, "/_/logout")]
#[tokio::test]
async fn test_authentication_routes_use_the_authentication_limit(#[case] method: Method, #[case] uri: &str) {
    let (_dir, state) = unwired_state_with_limits(RateLimitConfig {
        authentication: RouteLimit::new(1, 60),
        ..RateLimitConfig::enabled_defaults()
    });

    assert_eq!(
        request_statuses(&crate::router(state), method, uri).await[1],
        StatusCode::TOO_MANY_REQUESTS
    );
}

async fn request_statuses(app: &axum::Router, method: Method, uri: &str) -> [StatusCode; 2] {
    let mut statuses = [StatusCode::INTERNAL_SERVER_ERROR; 2];
    for status in &mut statuses {
        *status = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method.clone())
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status();
    }
    statuses
}

#[tokio::test]
async fn test_driver_mismatch_is_not_routable() {
    let mut other = test_index("other");
    other.ecosystem = peryx_core::Ecosystem::new("other");
    let (_dir, state) = unwired_state_with(vec![other]);
    let mut state = std::sync::Arc::into_inner(state).unwrap();
    register_indexed(&mut state, StubServing(peryx_core::Ecosystem::new("example")));
    let response = crate::router(std::sync::Arc::new(state))
        .oneshot(Request::builder().uri("/other/file").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[rstest]
#[case::post(Method::POST, "/alpha/", "post body", "POST /alpha/ application/json post body")]
#[case::put(Method::PUT, "/alpha/item", "put body", "PUT /alpha/item application/json put body")]
#[case::delete(Method::DELETE, "/alpha/item", "", "")]
#[tokio::test]
async fn test_registered_driver_handles_mutation(
    #[case] method: Method,
    #[case] uri: &str,
    #[case] body: &str,
    #[case] expected: &str,
) {
    let (_dir, state) = unwired_state_with(vec![test_index("alpha")]);
    let mut state = std::sync::Arc::into_inner(state).unwrap();
    register_indexed(&mut state, StubServing(peryx_core::Ecosystem::new("example")));
    let response = crate::router(std::sync::Arc::new(state))
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        expected
    );
}

#[rstest]
#[case::put(Method::PUT)]
#[case::delete(Method::DELETE)]
#[tokio::test]
async fn test_unknown_mutation_route_is_not_found(#[case] method: Method) {
    let (_dir, state) = unwired_state();
    let response = crate::router(state)
        .oneshot(
            Request::builder()
                .method(method)
                .uri("/missing/item")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[rstest]
#[case::discovery("/alpha/+api", StatusCode::OK)]
#[case::search("/alpha/+search?q=demo", StatusCode::OK)]
#[case::bad_search("/alpha/+search?availability=invalid", StatusCode::BAD_REQUEST)]
#[tokio::test]
async fn test_index_neutral_routes_precede_driver_dispatch(#[case] uri: &str, #[case] expected: StatusCode) {
    let (dir, state) = unwired_state_with(vec![test_index("alpha")]);
    let mut state = std::sync::Arc::into_inner(state).unwrap();
    register_indexed(&mut state, StubServing(peryx_core::Ecosystem::new("example")));
    let response = crate::router(std::sync::Arc::new(state))
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    drop(dir);
    assert_eq!(response.status(), expected);
}

#[tokio::test]
async fn test_root_search_rejects_an_invalid_filter() {
    let (_dir, state) = unwired_state();
    let response = crate::router(state)
        .oneshot(
            Request::builder()
                .uri("/+search?availability=invalid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_root_search_rejects_an_oversized_result_window() {
    let (_dir, state) = unwired_state();
    let response = crate::router(state)
        .oneshot(
            Request::builder()
                .uri("/+search?page=101&page_size=100")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            &axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap(),
        serde_json::json!({"error": "search page 101 with size 100 exceeds the 10000-result window"})
    );
}

#[rstest]
#[case::global("/+search?q=demo")]
#[case::index("/private/+search?q=demo")]
#[tokio::test]
async fn test_private_search_is_not_cached(#[case] uri: &str) {
    let mut index = test_index("private");
    index.acl.anonymous_read = false;
    let (_dir, state) = unwired_state_with(vec![index]);
    let response = crate::router(state)
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(
        (
            response.status(),
            response.headers()[axum::http::header::CACHE_CONTROL].to_str().unwrap(),
        ),
        (StatusCode::OK, "no-store")
    );
}

#[tokio::test]
async fn test_mixed_visibility_search_is_not_cached() {
    let mut private = test_index("private");
    private.acl.anonymous_read = false;
    let (_dir, state) = unwired_state_with(vec![test_index("public"), private]);
    let response = crate::router(state)
        .oneshot(Request::builder().uri("/+search?q=demo").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.headers()[axum::http::header::CACHE_CONTROL], "no-store");
}

#[tokio::test]
async fn test_public_search_keeps_its_cache_policy() {
    let (_dir, state) = unwired_state_with(vec![test_index("public"), test_index("mirror")]);
    let response = crate::router(state)
        .oneshot(Request::builder().uri("/+search?q=demo").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert!(!response.headers().contains_key(axum::http::header::CACHE_CONTROL));
}

#[tokio::test]
async fn test_private_search_error_is_not_cached() {
    let mut index = test_index("private");
    index.acl.anonymous_read = false;
    let (_dir, state) = state_with_search_storage_failure(vec![index]);
    let response = crate::router(state)
        .oneshot(Request::builder().uri("/+search?q=demo").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(
        (
            response.status(),
            response.headers()[axum::http::header::CACHE_CONTROL].to_str().unwrap(),
        ),
        (StatusCode::INTERNAL_SERVER_ERROR, "no-store")
    );
}

#[test]
fn test_search_internal_errors_are_server_errors() {
    let response = crate::handlers::search_error_response(&peryx_search::SearchError::Indexer("failed".to_owned()));
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[test]
fn test_search_response_maps_storage_failures() {
    let (_dir, state) = state_with_search_storage_failure(Vec::new());
    let services = HttpDomainServices::for_state(&state);
    let response = crate::handlers::search_response(&services, peryx_search::SearchParams::default(), None);
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[test]
fn test_a_driver_resolves_no_rate_limit_principal_by_default() {
    let (_dir, state) = unwired_state();
    assert!(
        state
            .rate_limit_principal_for(&peryx_core::Ecosystem::new("example"))
            .is_none()
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
        assert!(state.driver_for(&ecosystem).is_none(), "{ecosystem} was wired in");
    }
}

#[tokio::test]
async fn test_two_indexed_ecosystems_each_serve_their_own_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    let mut other = test_index("images");
    other.ecosystem = peryx_core::Ecosystem::new("other");
    let mut state = AppState::new(meta, blobs, 60, vec![test_index("alpha"), other]);
    // Registering a second driver must not displace the first: each keeps its own slot.
    register_indexed(&mut state, StubServing(peryx_core::Ecosystem::new("example")));
    register_indexed(&mut state, StubServing(peryx_core::Ecosystem::new("other")));
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

fn register_indexed(state: &mut AppState, driver: impl IndexedProtocolDriver + 'static) {
    state
        .register_protocol(
            ProtocolDriver::Indexed(std::sync::Arc::new(driver)),
            peryx_search::default_indexer(),
        )
        .unwrap();
}

fn register_absolute(state: &mut AppState, driver: impl AbsoluteProtocolDriver + 'static) {
    state
        .register_protocol(
            ProtocolDriver::Absolute(std::sync::Arc::new(driver)),
            peryx_search::default_indexer(),
        )
        .unwrap();
}

struct BareDriver;

impl peryx_driver::serving::EcosystemDriver for BareDriver {
    fn ecosystem(&self) -> peryx_core::Ecosystem {
        peryx_core::Ecosystem::new("example")
    }
}

#[test]
fn test_fixture_drivers_implement_required_contracts() {
    let description = peryx_driver::state::IndexDescription {
        name: "test".to_owned(),
        route: "test".to_owned(),
        ecosystem: "example".to_owned(),
        kind: "hosted",
        layers: Vec::new(),
        precedence: Vec::new(),
        uploads: false,
        volatile_deletes: false,
        upload_to: None,
        upstream: None,
        hosted: None,
    };
    let serving = StubServing(peryx_core::Ecosystem::new("example"));

    assert_eq!(
        serving.classify_route("test"),
        peryx_driver::rate_limit::RouteClass::Listing
    );
    assert_eq!(
        AbsoluteDriver.classify_route("test"),
        peryx_driver::rate_limit::RouteClass::Artifact
    );
    assert_eq!(BareDriver.ecosystem(), peryx_core::Ecosystem::new("example"));
    assert_eq!(description.ecosystem, "example");
}

#[tokio::test]
async fn test_unwired_state_search_returns_empty() {
    let (_dir, state) = unwired_state();
    let app = crate::router(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/+search?q=artifact-a")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let document: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(document["total"], 0);
    assert!(document["results"].as_array().unwrap().is_empty());
}
