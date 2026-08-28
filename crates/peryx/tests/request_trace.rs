use std::io::{Read as _, Seek as _};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use peryx_http::AppState;
use tower::ServiceExt as _;

#[tokio::test]
async fn request_trace_redacts_only_callback_query() {
    let directory = tempfile::tempdir().unwrap();
    let state = Arc::new(AppState::new(
        peryx_storage::meta::MetaStore::open(directory.path().join("peryx.redb")).unwrap(),
        peryx_storage::blob::BlobStore::new(directory.path().join("blobs")),
        60,
        Vec::new(),
    ));
    let mut capture = tempfile::tempfile().unwrap();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(Mutex::new(capture.try_clone().unwrap()))
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);

    let response = peryx_http::router(state.clone())
        .oneshot(
            Request::get(concat!(
                "/_/login/corporate/callback?error=access_denied&state=expected",
                "&error_description=provider-secret&error_uri=https%3A%2F%2Fprovider.example%2Fsecret"
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    let search = peryx_http::router(state)
        .oneshot(Request::get("/+search?q=visible-query").body(Body::empty()).unwrap())
        .await
        .unwrap();
    drop(guard);

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_eq!(search.status(), StatusCode::OK);
    capture.rewind().unwrap();
    let mut trace = String::new();
    capture.read_to_string(&mut trace).unwrap();
    assert!(trace.contains("uri=\"/_/login/corporate/callback\""), "{trace}");
    assert!(!trace.contains("provider-secret"), "{trace}");
    assert!(!trace.contains("error_description"), "{trace}");
    assert!(!trace.contains("error_uri"), "{trace}");
    assert!(trace.contains("uri=/+search?q=visible-query"), "{trace}");
}
