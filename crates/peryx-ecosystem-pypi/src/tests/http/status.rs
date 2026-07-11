//! The neutral `/+status`, `/+stats` and `/metrics` endpoints.

use super::support::*;

#[tokio::test]
async fn test_status_lists_routes() {
    let h = harness().await;
    let (status, headers, body) = get(&h.state, "/+status", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("json")
    );
    assert!(body.contains("root/pypi"));
    assert!(body.contains(env!("CARGO_PKG_VERSION")));
    assert!(body.contains(&h.server.uri()));
    assert!(!body.contains("\"project_count\""));
    assert!(!body.contains("\"upload_count\""));
    assert!(!body.contains("\"recent_uploads\""));
    assert!(!body.contains("s3cret"));
}
#[tokio::test]
async fn test_status_admin_details_include_bounded_summaries() {
    let h = harness().await;
    assert_eq!(
        upload_peryxpkg(&h.state, "/root/pypi/", &fixture_wheel()).await,
        StatusCode::OK
    );
    let (status, _, body) = get(&h.state, "/+status?details=admin", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"project_count\""));
    assert!(body.contains("\"upload_count\""));
    assert!(body.contains("\"recent_uploads\""));
    assert!(body.contains("peryxpkg-1.0-py3-none-any.whl"));
}
#[tokio::test]
async fn test_status_redacts_upstream_and_upload_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let indexes = vec![
        Index {
            name: "private".to_owned(),
            route: "private".to_owned(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Cached {
                client: UpstreamClient::with_auth(
                    "https://user:pass@example.invalid/simple/?token=url-secret#frag",
                    Auth::Bearer("bearer-secret".to_owned()),
                )
                .unwrap(),
                offline: false,
            },
            policy: Policy::default(),
        },
        Index {
            name: "hosted".to_owned(),
            route: "hosted".to_owned(),
            policy: Policy::default(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Hosted {
                upload_token: Some("upload-secret".to_owned()),
                volatile: false,
            },
        },
    ];
    let state = crate::tests::wired(AppState::new(meta, blobs, 60, indexes));
    let (status, _, body) = get(&state, "/+status", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("https://example.invalid/simple/"));
    assert!(body.contains("\"kind\":\"bearer\""));
    assert!(body.contains("<redacted>"));
    for secret in ["user", "pass", "url-secret", "bearer-secret", "upload-secret"] {
        assert!(!body.contains(secret));
    }
}
#[tokio::test]
async fn test_metrics_exposes_counters() {
    let h = harness().await;
    get(&h.state, "/+status", None).await;
    let (status, _, body) = get(&h.state, "/metrics", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("peryx_requests_total"));
    assert!(body.contains("peryx_index_metadata_total{index=\"pypi\",ecosystem=\"pypi\",role=\"cached\"} 0"));
}
#[tokio::test]
async fn test_metrics_exposes_per_index_counters() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_detail(&h.server, digest.as_str(), &file_url, None).await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    for _ in 0..500 {
        if h.state.metrics.index_totals().contains_key("pypi") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    // A second route makes the exposition ordering observable.
    h.state.metrics.record(peryx_events::metrics::Event::Page {
        route: "hosted".to_owned(),
        project: "veloxpkg".to_owned(),
    });
    for _ in 0..500 {
        if h.state.metrics.index_totals().len() == 2 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let (status, _, body) = get(&h.state, "/metrics", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("peryx_index_pages_total{index=\"hosted\",ecosystem=\"pypi\",role=\"hosted\"} 1"));
    assert!(body.contains("peryx_index_pages_total{index=\"pypi\",ecosystem=\"pypi\",role=\"cached\"} 1"));
    assert!(body.contains("peryx_index_refreshes_total{index=\"pypi\",ecosystem=\"pypi\",role=\"cached\"} 0"));
    assert!(body.contains("peryx_index_rejected_total{index=\"pypi\",ecosystem=\"pypi\",role=\"cached\"} 0"));
    // A caching-only counter never appears for the hosted index, and uploads never for the cache.
    assert!(!body.contains("peryx_index_refreshes_total{index=\"hosted\""));
    assert!(!body.contains("peryx_index_uploads_total{index=\"pypi\""));
}
