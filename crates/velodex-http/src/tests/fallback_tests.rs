//! The neutral no-op driver and indexer a state carries until an ecosystem is wired in.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use tower::ServiceExt as _;

use crate::state::AppState;

fn unwired_state() -> (tempfile::TempDir, std::sync::Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = velodex_storage::meta::MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    let blobs = velodex_storage::blob::BlobStore::new(dir.path().join("blobs"));
    (dir, std::sync::Arc::new(AppState::new(meta, blobs, 60, Vec::new())))
}

#[tokio::test]
async fn test_unwired_state_serves_503_on_every_method() {
    let (_dir, state) = unwired_state();
    let app = crate::router(state);
    let cases = [
        (Method::GET, "/pypi/simple/", Body::empty(), None),
        (Method::PUT, "/pypi/flask/1.0/yank", Body::empty(), None),
        (Method::DELETE, "/pypi/flask/1.0/", Body::empty(), None),
        (Method::GET, "/+api", Body::empty(), None),
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
