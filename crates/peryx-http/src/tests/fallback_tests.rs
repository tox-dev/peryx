//! The serving registry: what a state does before any ecosystem driver is wired in, and how it
//! keeps several route-mounted ecosystems apart once they are.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
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

fn pypi_index(route: &str) -> peryx_driver::state::Index {
    peryx_driver::state::Index {
        name: route.to_owned(),
        route: route.to_owned(),
        ecosystem: peryx_core::Ecosystem::Pypi,
        kind: peryx_driver::state::IndexKind::Hosted {
            upload_token: None,
            volatile: false,
        },
        policy: peryx_policy::Policy::default(),
    }
}

#[tokio::test]
async fn test_unwired_state_serves_503_when_a_driver_is_missing() {
    // A configured index with no ecosystem driver wired in: resolvable, so the request reaches the
    // driver seam and fails loudly rather than serving nothing.
    let (_dir, state) = unwired_state_with(vec![pypi_index("pypi")]);
    let app = crate::router(state);
    let cases = [
        (Method::GET, "/pypi/simple/", Body::empty(), None),
        (Method::PUT, "/pypi/flask/1.0/yank", Body::empty(), None),
        (Method::DELETE, "/pypi/flask/1.0/", Body::empty(), None),
        (
            Method::POST,
            "/pypi/",
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

#[tokio::test]
async fn test_unwired_discovery_renders_a_minimal_entry_per_index() {
    use peryx_core::Ecosystem;

    use peryx_driver::state::{Index, IndexKind};

    let dir = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    let index = Index {
        name: "pypi".to_owned(),
        route: "pypi".to_owned(),
        ecosystem: Ecosystem::Pypi,
        kind: IndexKind::Hosted {
            upload_token: None,
            volatile: false,
        },
        policy: peryx_policy::Policy::default(),
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
    assert_eq!(entry["route"], "pypi");
    assert_eq!(entry["ecosystem"], "pypi");
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
fn test_a_driver_publishes_no_metric_families_by_default() {
    use peryx_driver::serving::EcosystemDriver as _;

    assert!(StubServing(peryx_core::Ecosystem::Pypi).metric_families().is_empty());
}

#[tokio::test]
async fn test_a_driver_sweeps_nothing_by_default() {
    use peryx_driver::serving::{EcosystemDriver as _, RefreshSweep};

    let (_dir, state) = unwired_state();
    assert_eq!(
        StubServing(peryx_core::Ecosystem::Pypi)
            .refresh_stale(state.serving.clone())
            .await
            .unwrap(),
        RefreshSweep::default()
    );
}

#[test]
fn test_an_unwired_state_holds_no_driver_for_any_ecosystem() {
    let (_dir, state) = unwired_state();
    assert!(!state.has_any_driver());
    for ecosystem in peryx_core::Ecosystem::ALL {
        assert!(state.driver_for(*ecosystem).is_none(), "{ecosystem} was wired in");
    }
}

#[test]
fn test_serving_for_path_resolves_a_request_uri_path() {
    let dir = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    let mut state = AppState::new(meta, blobs, 60, vec![pypi_index("pypi")]);
    state.register_ecosystem(
        std::sync::Arc::new(StubServing(peryx_core::Ecosystem::Pypi)),
        std::sync::Arc::new(peryx_search::EmptyIndexer),
    );
    // A request URI path carries a leading slash; index routes do not. The rate limiter classes a
    // route by the driver this finds, so failing to resolve here silently downgrades every artifact
    // request to the listing limit.
    assert!(state.driver_for_path("/pypi/files/abc/x.whl").is_some());
    assert!(state.driver_for_path("/unconfigured/simple/").is_none());
}

#[tokio::test]
async fn test_two_route_mounted_ecosystems_each_serve_their_own_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    let mut oci = pypi_index("images");
    oci.ecosystem = peryx_core::Ecosystem::Oci;
    let mut state = AppState::new(meta, blobs, 60, vec![pypi_index("pypi"), oci]);
    // Registering a second driver must not displace the first: each keeps its own slot.
    state.register_ecosystem(
        std::sync::Arc::new(StubServing(peryx_core::Ecosystem::Pypi)),
        std::sync::Arc::new(peryx_search::EmptyIndexer),
    );
    state.register_ecosystem(
        std::sync::Arc::new(StubServing(peryx_core::Ecosystem::Oci)),
        std::sync::Arc::new(peryx_search::EmptyIndexer),
    );
    let app = crate::router(std::sync::Arc::new(state));

    for (route, expected) in [("pypi", "pypi"), ("images", "oci")] {
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
