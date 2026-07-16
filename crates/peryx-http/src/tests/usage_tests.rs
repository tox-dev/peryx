use axum::body::Body;
use axum::http::{Request, StatusCode};
use peryx_driver::state::AppState;
use peryx_events::metrics::Event;
use rstest::rstest;
use tower::ServiceExt as _;

fn state() -> (tempfile::TempDir, std::sync::Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = peryx_storage::blob::BlobStore::new(dir.path().join("blobs"));
    (dir, std::sync::Arc::new(AppState::new(meta, blobs, 60, Vec::new())))
}

async fn request(state: std::sync::Arc<AppState>, uri: &str) -> (StatusCode, serde_json::Value) {
    let response = crate::router(state)
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}

#[tokio::test]
async fn test_top_packages_returns_durable_usage() {
    let (_dir, state) = state();
    for (repository, project, filename, bytes) in [
        ("pypi", "flask", "flask-3.whl", 20),
        ("pypi", "flask", "flask-3.whl", 20),
        ("private", "django", "django-5.whl", 30),
    ] {
        state.metrics.record(Event::Download {
            route: repository.into(),
            project: project.into(),
            filename: filename.into(),
            bytes,
        });
    }
    let settled = (0..500).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(2));
        state.metrics.top_packages(2).len() == 2
    });
    assert!(settled, "metrics aggregator never settled");

    let (status, body) = request(state, "/+analytics/top-packages?limit=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        serde_json::json!([
            {"repository": "pypi", "project": "flask", "downloads": 2, "bytes": 40}
        ])
    );
}

#[rstest]
#[case::zero("/+analytics/top-packages?limit=0")]
#[case::too_large("/+analytics/top-packages?limit=101")]
#[tokio::test]
async fn test_top_packages_rejects_invalid_limits(#[case] uri: &str) {
    let (_dir, state) = state();
    let (status, body) = request(state, uri).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, serde_json::json!({"error": "limit must be between 1 and 100"}));
}

#[tokio::test]
async fn test_top_packages_defaults_to_twenty_five() {
    let (_dir, state) = state();
    for project in 0..26 {
        state.metrics.record(Event::Download {
            route: "pypi".into(),
            project: format!("project-{project:02}"),
            filename: "file.whl".into(),
            bytes: 1,
        });
    }
    let settled = (0..500).any(|_| {
        std::thread::sleep(std::time::Duration::from_millis(2));
        state.metrics.top_packages(26).len() == 26
    });
    assert!(settled, "metrics aggregator never settled");

    let (status, body) = request(state, "/+analytics/top-packages").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 25);
}
