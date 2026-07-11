//! Cross-cutting serving behavior.

use super::super::writable_index;
use super::support::*;

#[tokio::test]
async fn test_a_tag_staler_than_the_bound_is_not_served_when_upstream_fails() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};
    let server = MockServer::start().await;
    let manifest = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(manifest.to_vec(), MANIFEST_TYPE))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let now = Arc::new(AtomicI64::new(1000));
    let ticking = now.clone();
    let (_state, app) = crate::tests::proxy_with_clock(
        &tempfile::tempdir().unwrap(),
        &format!("{}/", server.uri()),
        Arc::new(move || ticking.load(Ordering::Relaxed)),
    );
    let uri = "/v2/hub/library/nginx/manifests/latest";
    assert_eq!(send(&app, Method::GET, uri).await.0, StatusCode::OK);

    // Upstream is down and the cached tag is older than the 60s window plus the 300s stale bound, so
    // the outage surfaces instead of a manifest of unbounded age.
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    now.store(1000 + 400, Ordering::Relaxed);
    let (status, _, _) = send(&app, Method::GET, uri).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}
#[tokio::test]
async fn test_a_zero_stale_bound_serves_a_tag_list_of_any_age() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/tags/list"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(br#"{"name":"library/nginx","tags":["1"]}"#.to_vec(), "application/json"),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let now = Arc::new(AtomicI64::new(1000));
    let ticking = now.clone();
    let (_state, app) = crate::tests::proxy_with_stale(
        &tempfile::tempdir().unwrap(),
        &format!("{}/", server.uri()),
        Arc::new(move || ticking.load(Ordering::Relaxed)),
        0,
    );
    let uri = "/v2/hub/library/nginx/tags/list";
    assert_eq!(send(&app, Method::GET, uri).await.0, StatusCode::OK);

    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/tags/list"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    // An operator mirroring a knowingly unreliable upstream asked for exactly this: no bound at all.
    now.store(1_000_000, Ordering::Relaxed);
    let (status, _, body) = send(&app, Method::GET, uri).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&body).contains("\"1\""));
}
#[tokio::test]
async fn test_expired_upstream_credentials_do_not_delete_a_cached_tag() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};
    let server = MockServer::start().await;
    let manifest = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    let uri = "/v2/hub/library/nginx/manifests/latest";
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(manifest.to_vec(), MANIFEST_TYPE))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let now = Arc::new(AtomicI64::new(1000));
    let ticking = now.clone();
    let (_state, app) = crate::tests::proxy_with_clock(
        &tempfile::tempdir().unwrap(),
        &format!("{}/", server.uri()),
        Arc::new(move || ticking.load(Ordering::Relaxed)),
    );
    assert_eq!(send(&app, Method::GET, uri).await.0, StatusCode::OK);

    // The token expires, so the revalidation after the freshness window is answered 401. Docker Hub
    // says that about a repository it will not discuss, not about one that is gone: the cached image
    // must still pull rather than become `manifest unknown`.
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    now.store(1000 + 61, Ordering::Relaxed);
    let (status, _, body) = send(&app, Method::GET, uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, manifest.to_vec());
}
#[tokio::test]
async fn test_hosted_tag_pointing_at_a_missing_manifest_is_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted(&dir);
    store::put_tag(&state.meta, "store", "app", "v1", &format!("sha256:{}", "c".repeat(64))).unwrap();
    let (status, _, body) = send(&app, Method::GET, "/v2/store/app/manifests/v1").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&body, "MANIFEST_UNKNOWN"), "{body:?}");
}
#[tokio::test]
async fn test_catalog_lists_oci_repositories_with_pagination() {
    let dir = tempfile::tempdir().unwrap();
    let pypi = peryx_index::Index {
        name: "py".to_owned(),
        route: "py".to_owned(),
        ecosystem: peryx_core::Ecosystem::Pypi,
        kind: peryx_index::IndexKind::Hosted { volatile: false },
        policy: peryx_policy::Policy::default(),
        acl: peryx_identity::IndexAcl::default(),
    };
    let hosted = |name: &str, route: &str| writable_index(name, route, true, "s3cret");
    // A root-route index lists its repos under bare names; a prefixed one under the route.
    let (_state, app) = app_with_indexes(&dir, vec![pypi, hosted("store", "store"), hosted("root", "")]);
    let manifest = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    for uri in [
        "/v2/store/app1/manifests/v1",
        "/v2/store/app2/manifests/v1",
        "/v2/bare/manifests/v1",
    ] {
        let (status, _, _) = crate::tests::send_body(
            &app,
            Method::PUT,
            uri,
            &[
                ("authorization", &crate::tests::auth("s3cret")),
                ("content-type", MANIFEST_TYPE),
            ],
            manifest.to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }

    // The catalog lists OCI repos as clients address them; the pypi index contributes nothing.
    let (status, _, body) = send(&app, Method::GET, "/v2/_catalog").await;
    assert_eq!(status, StatusCode::OK);
    let doc: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        doc["repositories"],
        serde_json::json!(["bare", "store/app1", "store/app2"])
    );

    let (status, headers, body) = send(&app, Method::GET, "/v2/_catalog?n=1").await;
    assert_eq!(status, StatusCode::OK);
    let doc: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(doc["repositories"], serde_json::json!(["bare"]));
    assert!(
        headers[header::LINK]
            .to_str()
            .unwrap()
            .contains("_catalog?n=1&last=bare")
    );
}
