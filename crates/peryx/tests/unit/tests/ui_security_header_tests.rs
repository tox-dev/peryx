//! The composed process router hands its browser defences to the pages and to the assets they load.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use peryx_driver::AppState;
use rstest::rstest;
use tower::ServiceExt as _;

use crate::server::router_for;
use crate::tests::support::render_gate;

fn composed() -> (tempfile::TempDir, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(AppState::new(
        peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        peryx_storage::blob::BlobStore::new(dir.path().join("blobs")),
        60,
        Vec::new(),
    ));
    (dir, router_for(state, axum::Router::new()))
}

async fn fetch(router: &axum::Router, uri: &str) -> axum::response::Response {
    router
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

fn value(response: &axum::response::Response, name: header::HeaderName) -> Option<&str> {
    response.headers().get(name).map(|value| value.to_str().unwrap())
}

#[rstest]
#[case::favicon("/favicon.svg")]
#[case::mark("/mark.svg")]
#[tokio::test]
async fn test_a_static_asset_refuses_content_sniffing(#[case] asset: &str) {
    let (_dir, router) = composed();

    let response = fetch(&router, asset).await;

    assert_eq!(
        (response.status(), value(&response, header::X_CONTENT_TYPE_OPTIONS)),
        (StatusCode::OK, Some("nosniff"))
    );
}

#[tokio::test]
async fn test_a_rendered_page_refuses_framing() {
    let (_dir, router) = composed();

    let response = {
        let _render = render_gate().lock().await;
        fetch(&router, "/").await
    };

    assert_eq!(
        (
            response.status(),
            value(&response, header::CONTENT_SECURITY_POLICY),
            value(&response, header::X_FRAME_OPTIONS),
            value(&response, header::REFERRER_POLICY),
            value(&response, header::X_CONTENT_TYPE_OPTIONS),
        ),
        (
            StatusCode::OK,
            Some("frame-ancestors 'none'; base-uri 'none'; object-src 'none'"),
            Some("DENY"),
            Some("no-referrer"),
            Some("nosniff"),
        )
    );
}
