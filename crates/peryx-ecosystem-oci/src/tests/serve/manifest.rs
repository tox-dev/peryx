//! Serving manifests by tag and digest, tag listing, and referrers.

use super::support::*;

#[tokio::test]
async fn test_manifest_by_tag_pulls_through_with_the_token_flow() {
    let server = MockServer::start().await;
    let body = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    // The 401 challenge is mounted first and the authenticated 200 last, so an anonymous request draws
    // the challenge and the token-bearing retry wins the tie.
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(
            ResponseTemplate::new(401).insert_header(
                "www-authenticate",
                format!(
                    r#"Bearer realm="{}/token",service="reg",scope="repository:library/nginx:pull""#,
                    server.uri()
                )
                .as_str(),
            ),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"token":"abc"}"#))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .and(match_header("authorization", "Bearer abc"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.to_vec(), MANIFEST_TYPE))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, headers, got) = send(&app, Method::GET, "/v2/hub/library/nginx/manifests/latest").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], MANIFEST_TYPE);
    assert_eq!(headers["docker-content-digest"], oci_digest(body));
    assert_eq!(got, &body[..]);
    // The tag mapping and manifest are cached under the canonical digest.
    assert_eq!(
        store::get_tag(&state.meta, "hub", "library/nginx", "latest").unwrap(),
        Some(oci_digest(body))
    );
}
#[tokio::test]
async fn test_unchanged_tag_revalidates_without_refetching_the_manifest() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};
    let server = MockServer::start().await;
    let manifest = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    let digest = oci_digest(manifest);
    // Exactly one GET: the cold pull. The revalidation after the window must be answered by the HEAD,
    // or wiremock's expect(1) fails on drop.
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(manifest.to_vec(), MANIFEST_TYPE))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).insert_header("docker-content-digest", digest.as_str()))
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

    now.store(1000 + 61, Ordering::Relaxed);
    let (status, _, body) = send(&app, Method::GET, uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, manifest.to_vec());
}
#[tokio::test]
async fn test_unchanged_tag_refetches_when_the_cached_manifest_is_missing() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};
    let server = MockServer::start().await;
    let manifest = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    let digest = oci_digest(manifest);
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(manifest.to_vec(), MANIFEST_TYPE))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .and(path("/v2/library/nginx/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).insert_header("docker-content-digest", digest.as_str()))
        .mount(&server)
        .await;
    let now = Arc::new(AtomicI64::new(1000));
    let ticking = now.clone();
    let (state, app) = crate::tests::proxy_with_clock(
        &tempfile::tempdir().unwrap(),
        &format!("{}/", server.uri()),
        Arc::new(move || ticking.load(Ordering::Relaxed)),
    );
    let uri = "/v2/hub/library/nginx/manifests/latest";
    assert_eq!(send(&app, Method::GET, uri).await.0, StatusCode::OK);

    // The tag has not moved, so the HEAD shortcut would serve from the store; the manifest it names is
    // gone, so it must fall through and fetch rather than answer with nothing.
    crate::store::delete_manifest(&state.meta, &digest).unwrap();
    now.store(1000 + 61, Ordering::Relaxed);
    let (status, _, body) = send(&app, Method::GET, uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, manifest.to_vec());
}
#[tokio::test]
async fn test_manifest_upstream_401_reports_the_auth_failure() {
    let server = MockServer::start().await;
    // The upstream refused peryx's credentials. With nothing cached to fall back on, that is what the
    // client is told: reporting the manifest unknown would name the wrong cause.
    Mock::given(method("GET"))
        .and(path("/v2/app/manifests/latest"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, _, body) = send(&app, Method::GET, "/v2/hub/app/manifests/latest").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(body_has_code(&body, "UNAUTHORIZED"), "{body:?}");
}
#[tokio::test]
async fn test_manifest_token_endpoint_failure_is_a_gateway_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/app/manifests/latest"))
        .respond_with(ResponseTemplate::new(401).insert_header(
            "www-authenticate",
            format!(r#"Bearer realm="{}/token",service="reg""#, server.uri()).as_str(),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, _, _) = send(&app, Method::GET, "/v2/hub/app/manifests/latest").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}
#[tokio::test]
async fn test_manifest_head_by_tag_returns_headers_only() {
    let server = MockServer::start().await;
    let body = br#"{"schemaVersion":2}"#;
    Mock::given(method("GET"))
        .and(path("/v2/app/manifests/v1"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.to_vec(), MANIFEST_TYPE))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, headers, got) = send(&app, Method::HEAD, "/v2/hub/app/manifests/v1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], oci_digest(body));
    assert_eq!(headers[header::CONTENT_LENGTH], body.len().to_string());
    assert!(got.is_empty());
}
#[tokio::test]
async fn test_manifest_by_digest_served_from_cache_without_upstream() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy(&dir, "http://127.0.0.1:1/", false);
    let body = br#"{"schemaVersion":2}"#;
    let digest = oci_digest(body);
    // Recorded as one `hub/app` serves, so the cached bytes answer without an upstream round trip.
    store::record_manifest(
        &state.meta,
        "hub",
        "app",
        &digest,
        &Manifest {
            media_type: MANIFEST_TYPE.to_owned(),
            bytes: body.to_vec(),
        },
    )
    .unwrap();
    let (status, headers, got) = send(&app, Method::GET, &format!("/v2/hub/app/manifests/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], digest);
    assert_eq!(got, &body[..]);
}
#[tokio::test]
async fn test_manifest_by_digest_pulls_through_and_verifies() {
    let server = MockServer::start().await;
    let body = br#"{"schemaVersion":2,"config":{}}"#;
    let digest = oci_digest(body);
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/manifests/{digest}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.to_vec(), MANIFEST_TYPE))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, _, got) = send(&app, Method::GET, &format!("/v2/hub/app/manifests/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &body[..]);
}
#[tokio::test]
async fn test_manifest_by_digest_mismatch_is_rejected() {
    let server = MockServer::start().await;
    let wrong = format!("sha256:{}", "b".repeat(64));
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/manifests/{wrong}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(b"different".to_vec(), MANIFEST_TYPE))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, _, body) = send(&app, Method::GET, &format!("/v2/hub/app/manifests/{wrong}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body_has_code(&body, "MANIFEST_INVALID"), "{body:?}");
}
#[tokio::test]
async fn test_manifest_by_digest_upstream_error_is_a_gateway_error() {
    let server = MockServer::start().await;
    let digest = format!("sha256:{}", "3".repeat(64));
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/manifests/{digest}")))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, _, _) = send(&app, Method::GET, &format!("/v2/hub/app/manifests/{digest}")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}
#[tokio::test]
async fn test_manifest_missing_upstream_is_unknown() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/app/manifests/absent"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, _, body) = send(&app, Method::GET, "/v2/hub/app/manifests/absent").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&body, "MANIFEST_UNKNOWN"), "{body:?}");
}
#[tokio::test]
async fn test_tags_list_passes_upstream_through() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/library/nginx/tags/list"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            br#"{"name":"library/nginx","tags":["1.25","latest"]}"#.to_vec(),
            "application/json",
        ))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, _, body) = send(&app, Method::GET, "/v2/hub/library/nginx/tags/list").await;
    assert_eq!(status, StatusCode::OK);
    assert!(std::str::from_utf8(&body).unwrap().contains("\"1.25\""));
}
#[tokio::test]
async fn test_tags_list_from_cache_when_hosted() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted(&dir);
    let digest = format!("sha256:{}", "a".repeat(64));
    store::put_tag(&state.meta, "store", "app", "latest", &digest).unwrap();
    store::put_tag(&state.meta, "store", "app", "v2", &digest).unwrap();
    let (status, _, body) = send(&app, Method::GET, "/v2/store/app/tags/list").await;
    assert_eq!(status, StatusCode::OK);
    let text = std::str::from_utf8(&body).unwrap();
    assert!(text.contains("\"store/app\""), "{text}");
    assert!(text.contains("\"latest\"") && text.contains("\"v2\""), "{text}");
}
#[tokio::test]
async fn test_manifest_upstream_unreachable_is_a_gateway_error() {
    let dir = tempfile::tempdir().unwrap();
    // An online proxy whose upstream refuses every connection surfaces a transport fault as 502.
    let (_state, app) = proxy(&dir, "http://127.0.0.1:1/", false);
    let (status, _, _) = send(&app, Method::GET, "/v2/hub/app/manifests/latest").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}
#[tokio::test]
async fn test_manifest_by_digest_missing_on_hosted_is_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted(&dir);
    let digest = format!("sha256:{}", "e".repeat(64));
    let (status, _, body) = send(&app, Method::GET, &format!("/v2/store/app/manifests/{digest}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&body, "MANIFEST_UNKNOWN"), "{body:?}");
}
#[tokio::test]
async fn test_manifest_by_digest_missing_upstream_is_unknown() {
    let server = MockServer::start().await;
    let digest = format!("sha256:{}", "1".repeat(64));
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/manifests/{digest}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, _, body) = send(&app, Method::GET, &format!("/v2/hub/app/manifests/{digest}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&body, "MANIFEST_UNKNOWN"), "{body:?}");
}
#[tokio::test]
async fn test_referrers_merge_upstream_and_filter_by_artifact_type() {
    let server = MockServer::start().await;
    let subject = format!("sha256:{}", "a".repeat(64));
    let sig = format!("sha256:{}", "b".repeat(64));
    // Upstream lists the signature twice (dedup) and one descriptor with no digest (skipped).
    let index = format!(
        concat!(
            r#"{{"schemaVersion":2,"manifests":["#,
            r#"{{"digest":"{sig}","artifactType":"application/vnd.example.sig"}},"#,
            r#"{{"digest":"{sig}","artifactType":"application/vnd.example.sig"}},"#,
            r#"{{"artifactType":"application/vnd.example.sig"}}]}}"#,
        ),
        sig = sig,
    );
    Mock::given(method("GET"))
        .and(path(format!("/v2/library/nginx/referrers/{subject}")))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(index.into_bytes(), "application/vnd.oci.image.index.v1+json"),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let base = format!("/v2/hub/library/nginx/referrers/{subject}");

    let (status, headers, body) = send(&app, Method::GET, &base).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key("oci-filters-applied"));
    let doc: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(doc["manifests"].as_array().unwrap().len(), 1);
    assert_eq!(doc["manifests"][0]["digest"], sig);

    let (status, headers, body) = send(
        &app,
        Method::GET,
        &format!("{base}?artifactType=application/vnd.example.sig"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["oci-filters-applied"], "artifactType");
    let doc: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(doc["manifests"].as_array().unwrap().len(), 1);

    let (_, _, body) = send(&app, Method::GET, &format!("{base}?artifactType=application/vnd.other")).await;
    let doc: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(doc["manifests"].as_array().unwrap().is_empty());
}
#[tokio::test]
async fn test_referrers_tolerate_an_upstream_without_the_api() {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let subject = format!("sha256:{}", "c".repeat(64));
    let (status, _, body) = send(&app, Method::GET, &format!("/v2/hub/library/nginx/referrers/{subject}")).await;
    assert_eq!(status, StatusCode::OK);
    let doc: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(doc["manifests"].as_array().unwrap().is_empty());
}
