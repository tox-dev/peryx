//! The neutral `/+status`, `/+stats` and `/metrics` endpoints.

use super::support::*;
use peryx_driver::rate_limit::UpstreamLimits;
use peryx_identity::IndexAcl;
use peryx_upstream::{NamedUpstream, UpstreamRouter};

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

#[rstest]
#[case::liveness("/+health", r#"{"status":"live"}"#)]
#[case::readiness("/+ready", r#"{"status":"ready"}"#)]
#[tokio::test]
async fn test_public_probe_has_a_fixed_redacted_document(#[case] uri: &str, #[case] expected: &str) {
    let h = harness().await;
    let (status, headers, body) = get(&h.state, uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    assert_eq!(body, expected);
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
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
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
            acl: IndexAcl::default(),
        },
        Index {
            name: "hosted".to_owned(),
            route: "hosted".to_owned(),
            policy: Policy::default(),
            acl: IndexAcl::upload_token("upload-secret".to_owned()),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Hosted { volatile: false },
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
async fn test_status_reports_routed_upstream_health() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let primary = NamedUpstream::new(
        "primary",
        UpstreamClient::with_auth(
            "https://user:pass@primary.example/simple/?token=url-secret#frag",
            Auth::Bearer("bearer-secret".to_owned()),
        )
        .unwrap(),
    );
    primary.mark_unhealthy();
    let fallback = NamedUpstream::new(
        "fallback",
        UpstreamClient::new("https://fallback.example/simple/").unwrap(),
    );
    fallback.mark_healthy();
    let mut state = AppState::new(
        meta,
        blobs,
        60,
        vec![Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Cached {
                client: primary.client().clone(),
                offline: false,
            },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        }],
    );
    state
        .upstream_routes
        .insert("pypi".to_owned(), UpstreamRouter::new(vec![primary, fallback]).unwrap());
    let state = crate::tests::wired(state);

    let (status, _, body) = get(&state, "/+status", None).await;
    assert_eq!(status, StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&body).unwrap();
    let upstream = &body["indexes"][0]["upstream"];
    assert_eq!(upstream["status"], "degraded");
    assert_eq!(
        upstream["sources"],
        serde_json::json!([
            {
                "name": "primary",
                "url": "https://primary.example/simple/",
                "auth": {"kind": "bearer", "redacted": "<redacted>"},
                "status": "unhealthy",
            },
            {
                "name": "fallback",
                "url": "https://fallback.example/simple/",
                "auth": {"kind": "none", "redacted": null},
                "status": "healthy",
            },
        ])
    );
}

#[tokio::test]
async fn test_metrics_exposes_counters() {
    let h = harness().await;
    get(&h.state, "/+status", None).await;
    let (status, _, body) = get(&h.state, "/metrics", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("peryx_requests_total"));
    assert!(body.contains("peryx_metadata_served_total{ecosystem=\"pypi\",role=\"cached\"} 0"));
}
#[tokio::test]
async fn test_metrics_exposes_bounded_role_counters() {
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
    h.state.metrics.record(peryx_events::metrics::Event::CatalogSync {
        route: "pypi".to_owned(),
        outcome: peryx_events::metrics::CatalogSyncOutcome::Published,
        projects: Some(700_000),
    });
    for _ in 0..500 {
        let totals = h.state.metrics.index_totals();
        if totals.len() == 2
            && totals
                .get("pypi")
                .is_some_and(|counters| counters.cached.catalog_syncs == 1)
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let (status, _, body) = get(&h.state, "/metrics", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("peryx_pages_served_total{ecosystem=\"pypi\",role=\"hosted\"} 1"));
    assert!(body.contains("peryx_pages_served_total{ecosystem=\"pypi\",role=\"cached\"} 1"));
    assert!(body.contains("peryx_upstream_refreshes_total{ecosystem=\"pypi\",role=\"cached\"} 0"));
    assert!(body.contains("peryx_artifacts_rejected_total{ecosystem=\"pypi\",role=\"cached\"} 0"));
    assert!(body.contains("# TYPE peryx_catalog_projects gauge"));
    assert!(body.contains("peryx_catalog_syncs_total{ecosystem=\"pypi\",role=\"cached\"} 1"));
    assert!(body.contains("peryx_catalog_projects{ecosystem=\"pypi\",role=\"cached\"} 700000"));
    // A caching-only counter never appears for the hosted index, and uploads never for the cache.
    assert!(!body.contains("peryx_upstream_refreshes_total{ecosystem=\"pypi\",role=\"hosted\""));
    assert!(!body.contains("peryx_artifacts_uploaded_total{ecosystem=\"pypi\",role=\"cached\""));
}

#[tokio::test]
async fn test_metrics_omit_hostile_values_and_bound_series_count() {
    let dir = tempfile::tempdir().unwrap();
    let indexes: Vec<_> = (0..64)
        .map(|position| Index {
            name: format!("repository-credential-{position}"),
            route: format!("repository-credential-{position}"),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Hosted { volatile: false },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        })
        .collect();
    let mut app = AppState::new(
        MetaStore::open(dir.path().join("peryx.redb")).unwrap(),
        BlobStorage::filesystem(dir.path().join("blobs")),
        60,
        indexes,
    );
    app.upstream_limits = UpstreamLimits::new([(
        "https://user:pass@example.invalid/simple?X-Amz-Credential=actor&X-Amz-Signature=signed-secret".to_owned(),
        1,
    )]);
    let state = crate::tests::wired(app);
    for position in 0..64 {
        state.metrics.record(peryx_events::metrics::Event::Download {
            route: format!("repository-credential-{position}"),
            project: "actor-token-value".to_owned(),
            filename: "../../private/path?error=raw-secret".to_owned(),
            version: None,
            source: None,
            bytes: 1,
        });
    }
    for _ in 0..500 {
        if state.metrics.index_totals().len() == 64 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }

    let (status, _, body) = get(&state, "/metrics", None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body.lines()
            .filter(|line| line.starts_with("peryx_artifacts_served_total{"))
            .count(),
        1
    );
    assert!(body.contains("peryx_artifacts_served_total{ecosystem=\"pypi\",role=\"hosted\"} 64"));
    assert!(body.contains("peryx_upstream_rate_limit_denied_total 0"));
    for secret in [
        "repository-credential",
        "user:pass",
        "X-Amz-Credential",
        "signed-secret",
        "actor-token-value",
        "private/path",
        "raw-secret",
    ] {
        assert!(!body.contains(secret), "{secret} leaked into metrics:\n{body}");
    }
}
