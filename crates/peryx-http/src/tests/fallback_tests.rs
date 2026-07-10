//! The neutral no-op driver and indexer a state carries until an ecosystem is wired in.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use tower::ServiceExt as _;

use crate::state::AppState;

fn unwired_state() -> (tempfile::TempDir, std::sync::Arc<AppState>) {
    unwired_state_with(Vec::new())
}

fn unwired_state_with(indexes: Vec<crate::state::Index>) -> (tempfile::TempDir, std::sync::Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    (dir, std::sync::Arc::new(AppState::new(meta, blobs, 60, indexes)))
}

fn pypi_index(route: &str) -> crate::state::Index {
    crate::state::Index {
        name: route.to_owned(),
        route: route.to_owned(),
        ecosystem: peryx_core::Ecosystem::Pypi,
        kind: crate::state::IndexKind::Hosted {
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

    use crate::state::{Index, IndexKind};

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

#[test]
fn test_unconfigured_serving_classifies_index_routes_as_listing() {
    use crate::rate_limit::RouteClass;
    use crate::serving::{EcosystemServing as _, UnconfiguredServing};

    assert_eq!(UnconfiguredServing.classify_route("/pypi/simple/"), RouteClass::Listing);
    assert_eq!(
        UnconfiguredServing.classify_route("/pypi/files/abc/x.whl"),
        RouteClass::Listing
    );
}

#[test]
fn test_unconfigured_serving_publishes_no_metric_families() {
    use crate::serving::{EcosystemServing as _, UnconfiguredServing};

    assert!(UnconfiguredServing.metric_families().is_empty());
}

#[tokio::test]
async fn test_unconfigured_serving_sweeps_nothing() {
    use crate::serving::{EcosystemServing as _, RefreshSweep, UnconfiguredServing};

    let (_dir, state) = unwired_state();
    assert_eq!(
        UnconfiguredServing.refresh_stale(state).await.unwrap(),
        RefreshSweep::default()
    );
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
