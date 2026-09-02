use std::io::{Read as _, Seek as _};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use peryx_driver::AppState;
use peryx_driver::rate_limit::{RateLimitConfig, RouteLimit};
use rstest::rstest;
use tower::ServiceExt as _;

use crate::server::router_for;
use crate::tests::support::render_gate;

fn limited(limits: RateLimitConfig) -> (tempfile::TempDir, Arc<AppState>, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let state = Arc::new(AppState::with_rate_limits(
        peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        peryx_storage::blob::BlobStore::new(dir.path().join("blobs")),
        60,
        Vec::new(),
        limits,
        std::iter::empty(),
    ));
    let router = router_for(Arc::clone(&state), axum::Router::new());
    (dir, state, router)
}

async fn status(router: &axum::Router, uri: &str) -> StatusCode {
    router
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

#[rstest]
#[case::search("/search?q=numpy&page_size=100")]
#[case::browse("/browse?index=main")]
#[tokio::test]
async fn test_ui_listing_page_shares_the_json_listing_budget(#[case] page: &str) {
    let (_dir, _state, router) = limited(RateLimitConfig {
        listing: RouteLimit::new(1, 60),
        ..RateLimitConfig::enabled_defaults()
    });

    let json = status(&router, "/+search?q=numpy").await;
    let rendered = status(&router, page).await;

    assert_eq!((json, rendered), (StatusCode::OK, StatusCode::TOO_MANY_REQUESTS));
}

#[rstest]
#[case::dashboard("/")]
#[case::admin_status("/admin/status")]
#[case::stats("/stats")]
#[tokio::test]
async fn test_ui_admin_page_shares_the_json_admin_budget(#[case] page: &str) {
    let (_dir, _state, router) = limited(RateLimitConfig {
        admin: RouteLimit::new(1, 60),
        ..RateLimitConfig::enabled_defaults()
    });

    let json = status(&router, "/+status").await;
    let rendered = status(&router, page).await;

    assert_eq!((json, rendered), (StatusCode::OK, StatusCode::TOO_MANY_REQUESTS));
}

#[tokio::test]
async fn test_ui_login_page_shares_the_authentication_budget() {
    let (_dir, _state, router) = limited(RateLimitConfig {
        authentication: RouteLimit::new(1, 60),
        ..RateLimitConfig::enabled_defaults()
    });

    let session = status(&router, "/_/session").await;
    let rendered = status(&router, "/login").await;

    assert_eq!((session, rendered), (StatusCode::OK, StatusCode::TOO_MANY_REQUESTS));
}

#[rstest]
#[case::favicon("/favicon.svg")]
#[case::mark("/mark.svg")]
#[tokio::test]
async fn test_ui_static_assets_stay_outside_every_budget(#[case] asset: &str) {
    let (_dir, _state, router) = limited(RateLimitConfig {
        listing: RouteLimit::new(1, 60),
        admin: RouteLimit::new(1, 60),
        ..RateLimitConfig::enabled_defaults()
    });
    status(&router, "/+search?q=numpy").await;
    status(&router, "/+status").await;

    let first = status(&router, asset).await;
    let second = status(&router, asset).await;

    assert_eq!((first, second), (StatusCode::OK, StatusCode::OK));
}

#[tokio::test]
async fn test_ui_static_assets_do_not_advance_the_request_counter() {
    let (_dir, state, router) = limited(RateLimitConfig::default());

    let before = state.serving.requests.load(Ordering::Relaxed);
    status(&router, "/favicon.svg").await;

    assert_eq!(state.serving.requests.load(Ordering::Relaxed), before);
}

#[tokio::test]
async fn test_ui_page_render_advances_the_request_counter() {
    let (_dir, state, router) = limited(RateLimitConfig::default());

    let before = state.serving.requests.load(Ordering::Relaxed);
    let rendered = {
        let _render = render_gate().lock().await;
        status(&router, "/browse?index=main").await
    };

    assert_eq!(
        (rendered, state.serving.requests.load(Ordering::Relaxed)),
        (StatusCode::OK, before + 1)
    );
}

#[test]
fn test_ui_page_render_emits_the_request_span_and_response_event() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut log = tempfile::tempfile().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::sync::Mutex::new(log.try_clone().unwrap()))
        .finish();

    tracing::subscriber::with_default(subscriber, || {
        runtime.block_on(async {
            let (_dir, _state, router) = limited(RateLimitConfig::default());
            let _render = render_gate().lock().await;
            assert_eq!(status(&router, "/browse?index=main").await, StatusCode::OK);
        });
    });

    let mut text = String::new();
    log.rewind().unwrap();
    log.read_to_string(&mut text).unwrap();
    assert!(text.contains("request{method=GET uri=/browse?index=main"), "{text}");
    assert!(text.contains("finished processing request"), "{text}");
}
