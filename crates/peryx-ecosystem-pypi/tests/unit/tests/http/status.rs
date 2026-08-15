use super::support::*;
use peryx_driver::rate_limit::UpstreamLimits;
use peryx_identity::IndexAcl;
use peryx_upstream::{NamedUpstream, UpstreamRouter};

async fn get_admin(state: &Arc<AppState>, uri: &str) -> (StatusCode, HeaderMap, String) {
    let authorization = crate::tests::administrator_header(state).await;
    let (status, headers, bytes) =
        get_bytes_with_headers(state, uri, &[(header::AUTHORIZATION.as_str(), &authorization)]).await;
    (status, headers, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn test_status_lists_routes_for_an_administrator() {
    let h = harness().await;
    let (status, headers, body) = get_admin(&h.state, "/+status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], "private, no-cache");
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
    assert!(!body.contains("s3cret"));
}

#[tokio::test]
async fn test_status_withholds_sensitive_index_fields_from_anonymous() {
    let h = harness().await;
    let (status, headers, body) = get(&h.state, "/+status", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], "private, no-cache");
    assert!(body.contains(env!("CARGO_PKG_VERSION")));

    assert!(body.contains("root/pypi"), "{body}");

    assert!(!body.contains(&h.server.uri()), "{body}");
    assert!(!body.contains("\"upstream\""), "{body}");
    assert!(!body.contains("\"upload_token\""), "{body}");
    assert!(!body.contains("\"resource_count\""), "{body}");
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
    let (status, _, body) = get_admin(&h.state, "/+status").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"resource_count\""));
    assert!(body.contains("\"write_count\""));
    assert!(body.contains("\"recent_writes\""));
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
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Cached {
                client: UpstreamClient::with_auth(
                    "https://upstream-account-secret:upstream-password-secret@example.invalid/simple/?token=url-secret#frag",
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
            acl: crate::tests::writer_acl("upload-secret".to_owned()),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Hosted { volatile: false },
        },
    ];
    let state = crate::tests::wired(AppState::new(meta, blobs, 60, indexes));
    let (status, _, body) = get_admin(&state, "/+status").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("https://example.invalid/simple/"));
    assert!(body.contains("\"kind\":\"bearer\""));
    assert!(body.contains("<redacted>"));
    for secret in [
        "upstream-account-secret",
        "upstream-password-secret",
        "url-secret",
        "bearer-secret",
        "upload-secret",
    ] {
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
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Cached {
                client: primary.client().clone(),
                offline: false,
            },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        }],
    );
    Arc::get_mut(&mut state.serving)
        .unwrap()
        .upstream_routes
        .insert("pypi".to_owned(), UpstreamRouter::new(vec![primary, fallback]).unwrap());
    let state = crate::tests::wired(state);

    let (status, _, body) = get_admin(&state, "/+status").await;
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
    h.state.serving.metrics.flush().unwrap();

    h.state
        .serving
        .metrics
        .record(peryx_events::metrics::Observation::Page {
            repository: "hosted".to_owned(),
            resource: "veloxpkg".to_owned(),
        });
    crate::catalog_job::record_catalog_metrics(
        &h.state.serving.metrics,
        "pypi",
        crate::catalog_job::CatalogMetricOutcome::Published { projects: 700_000 },
    );
    h.state.serving.metrics.flush().unwrap();
    let (status, _, body) = get(&h.state, "/metrics", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("peryx_pages_served_total{ecosystem=\"pypi\",role=\"hosted\"} 1"));
    assert!(body.contains("peryx_pages_served_total{ecosystem=\"pypi\",role=\"cached\"} 1"));
    assert!(body.contains("peryx_upstream_refreshes_total{ecosystem=\"pypi\",role=\"cached\"} 0"));
    assert!(body.contains("peryx_artifacts_rejected_total{ecosystem=\"pypi\",role=\"cached\"} 0"));
    assert!(body.contains("# TYPE peryx_catalog_projects gauge"));
    assert!(body.contains("peryx_catalog_syncs_total{ecosystem=\"pypi\",role=\"cached\"} 1"));
    assert!(body.contains("peryx_catalog_projects{ecosystem=\"pypi\",role=\"cached\"} 700000"));

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
            ecosystem: crate::ECOSYSTEM,
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
    Arc::get_mut(&mut app.serving).unwrap().upstream_limits = UpstreamLimits::new([(
        "https://user:pass@example.invalid/simple?X-Amz-Credential=actor&X-Amz-Signature=signed-secret".to_owned(),
        1,
    )]);
    let state = crate::tests::wired(app);
    for position in 0..64 {
        state.serving.metrics.record(peryx_events::metrics::Observation::Read {
            repository: format!("repository-credential-{position}"),
            resource: "actor-token-value".to_owned(),
            artifact: "../../private/path?error=raw-secret".to_owned(),
            group: None,
            source: None,
            bytes: 1,
        });
    }
    state.serving.metrics.flush().unwrap();

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
