//! The `/+api` discovery document, per index and per ecosystem.

use super::support::*;
use peryx_identity::IndexAcl;

#[tokio::test]
async fn test_discovery_document_ignores_untrusted_forwarding_and_redacts_token() {
    let h = harness().await;
    let (status, body) = get_with_headers(
        &h.state,
        "/+api",
        &[
            ("host", "internal.local"),
            ("x-forwarded-host", "packages.example"),
            ("x-forwarded-proto", "https"),
        ],
    )
    .await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let indexes = json["indexes"].as_array().unwrap();
    let virtual_index = indexes.iter().find(|index| index["route"] == "root/pypi").unwrap();
    let cached = indexes.iter().find(|index| index["route"] == "pypi").unwrap();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json["urls"],
        serde_json::json!({
            "api": "http://internal.local/+api",
            "health": "http://internal.local/+health",
            "readiness": "http://internal.local/+ready",
            "status": "http://internal.local/+status",
            "stats": "http://internal.local/+stats",
            "openapi": "http://internal.local/api-docs/openapi.json",
            "web": "http://internal.local/"
        })
    );
    assert_eq!(
        virtual_index["urls"],
        serde_json::json!({
            "api": "http://internal.local/root/pypi/+api",
            "simple": "http://internal.local/root/pypi/simple/",
            "upload": "http://internal.local/root/pypi/",
            "status": "http://internal.local/+status",
            "web": "http://internal.local/browse?index=root%2Fpypi",
            "stats": "http://internal.local/stats?index=root%2Fpypi",
            "openapi": "http://internal.local/api-docs/openapi.json"
        })
    );
    assert_eq!(
        virtual_index["capabilities"],
        serde_json::json!({
            "simple_html": true,
            "simple_json": true,
            "simple_api_version": "1.4",
            "metadata_siblings": true,
            "uploads": true,
            "yanking": true,
            "volatile_deletes": true,
            "project_status": true,
            "provenance": true,
            "legacy_json": true
        })
    );
    assert_eq!(cached["urls"].get("upload"), None);
    assert_eq!(cached["client_configuration"].get(".pypirc"), None);
    assert_eq!(cached["capabilities"]["uploads"], false);
    assert_eq!(cached["capabilities"]["yanking"], false);
    assert_eq!(cached["capabilities"]["volatile_deletes"], false);
    assert!(body.contains("\"uv.toml\""));
    assert!(body.contains("password = <upload-token>"));
    assert!(!body.contains("s3cret"));
}
#[tokio::test]
async fn test_discovery_lists_every_ecosystem_with_its_own_driver() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let indexes = vec![
        Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Hosted { volatile: true },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        },
        Index {
            name: "images".to_owned(),
            route: "images".to_owned(),
            ecosystem: peryx_core::Ecosystem::Oci,
            kind: IndexKind::Hosted { volatile: true },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        },
    ];
    // No OCI driver is wired here, so the OCI index falls back to the neutral driver's minimal entry:
    // it still appears in the document, but without the registry URLs a real driver would render.
    let state = crate::tests::wired(AppState::with_clock(meta, blobs, 60, indexes, Arc::new(|| 1000)));
    let (status, body) = get_with_headers(&state, "/+api", &[("host", "127.0.0.1:4433")]).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let indexes = json["indexes"].as_array().unwrap();
    let routes: Vec<&str> = indexes.iter().map(|index| index["route"].as_str().unwrap()).collect();
    assert_eq!(routes, ["pypi", "images"]);

    let pypi = &indexes[0];
    assert_eq!(pypi["ecosystem"], "pypi");
    assert!(pypi["urls"]["simple"].is_string());

    let oci = &indexes[1];
    assert_eq!(oci["ecosystem"], "oci");
    assert_eq!(
        oci["urls"],
        serde_json::Value::Null,
        "the neutral fallback renders no URLs"
    );
}
#[tokio::test]
async fn test_per_index_discovery_dispatches_an_oci_index_to_the_oci_driver() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let indexes = vec![Index {
        name: "images".to_owned(),
        route: "images".to_owned(),
        ecosystem: peryx_core::Ecosystem::Oci,
        kind: IndexKind::Hosted { volatile: true },
        policy: Policy::default(),
        acl: IndexAcl::default(),
    }];
    // The PyPI dispatch handles the neutral `/{route}/+api` route for every index, delegating an OCI
    // index's entry to the OCI driver rather than rendering a Simple-API document for it.
    let state = crate::tests::wired(AppState::with_clock(meta, blobs, 60, indexes, Arc::new(|| 1000)));
    let (status, body) = get_with_headers(&state, "/images/+api", &[("host", "127.0.0.1:4433")]).await;
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["index"]["route"], "images");
    assert_eq!(json["index"]["ecosystem"], "oci");
    assert_eq!(json["index"]["urls"], serde_json::Value::Null);
}
#[tokio::test]
async fn test_index_discovery_route_accepts_trailing_slash() {
    let h = harness().await;
    let (status, body) = get_with_headers(&h.state, "/root/pypi/+api/", &[("host", "127.0.0.1:4433")]).await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["index"]["route"], "root/pypi");
    assert_eq!(
        json["index"]["urls"]["simple"],
        "http://127.0.0.1:4433/root/pypi/simple/"
    );
}
#[tokio::test]
async fn test_index_discovery_unknown_route_is_not_found() {
    let h = harness().await;
    let (status, ..) = get(&h.state, "/missing/+api", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
#[tokio::test]
async fn test_openapi_endpoint_serves_the_document() {
    let h = harness().await;
    let (status, headers, body) = get(&h.state, "/api-docs/openapi.json", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "application/json");
    let spec: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(spec["openapi"], "3.1.0");
}
