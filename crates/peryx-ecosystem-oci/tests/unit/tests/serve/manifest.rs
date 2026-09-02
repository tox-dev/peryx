use super::support::*;
use crate::store::MAX_MEDIA_TYPE_BYTES;
use crate::tests::observe_pending;

#[tokio::test]
async fn test_manifest_by_tag_pulls_through_with_the_token_flow() {
    let server = MockServer::start().await;
    let body = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    // Mock ordering keeps anonymous and authenticated responses distinct.
    Mock::given(pull("/v2/library/nginx/manifests/latest"))
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
    Mock::given(pull("/v2/library/nginx/manifests/latest"))
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
    assert_eq!(
        store::get_tag(&state.serving.meta, "hub", "library/nginx", "latest").unwrap(),
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

    assert_eq!(
        state
            .serving
            .meta
            .remove_driver_values_if("oci\0m\0", 1, |_| Ok(true))
            .unwrap()
            .len(),
        1,
    );
    now.store(1000 + 61, Ordering::Relaxed);
    let (status, _, body) = send(&app, Method::GET, uri).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, manifest.to_vec());
}
#[tokio::test]
async fn test_manifest_upstream_401_reports_the_auth_failure() {
    let server = MockServer::start().await;
    // Authentication failures must not be reported as missing manifests.
    Mock::given(pull("/v2/app/manifests/latest"))
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
    Mock::given(pull("/v2/app/manifests/latest"))
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
    mount_head_without_digest(&server, "/v2/app/manifests/v1").await;
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
    store::record_manifest(
        &state.serving.meta,
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
async fn test_manifest_at_the_media_type_limit_serves_bytes_matching_its_digest() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy(&dir, "http://127.0.0.1:1/", false);
    let body = b"body";
    let digest = oci_digest(body);
    store::record_manifest(
        &state.serving.meta,
        "hub",
        "app",
        &digest,
        &Manifest {
            media_type: "a".repeat(MAX_MEDIA_TYPE_BYTES),
            bytes: body.to_vec(),
        },
    )
    .unwrap();

    let (status, headers, got) = send(&app, Method::GET, &format!("/v2/hub/app/manifests/{digest}")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["docker-content-digest"], oci_digest(&got));
    assert_eq!(got, &body[..]);
}
#[tokio::test]
async fn test_manifest_media_type_over_the_storage_limit_is_not_cached() {
    let server = MockServer::start().await;
    let body = b"body";
    Mock::given(method("GET"))
        .and(path("/v2/app/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.to_vec(), &"a".repeat(MAX_MEDIA_TYPE_BYTES + 1)))
        .mount(&server)
        .await;
    mount_head_without_digest(&server, "/v2/app/manifests/latest").await;
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy(&dir, &format!("{}/", server.uri()), false);

    let (status, _, _) = send(&app, Method::GET, "/v2/hub/app/manifests/latest").await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(
        (
            store::get_manifest(&state.serving.meta, &oci_digest(body)).unwrap(),
            store::get_tag(&state.serving.meta, "hub", "app", "latest").unwrap(),
        ),
        (None, None)
    );
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
async fn test_manifest_by_digest_mismatch_does_not_cache_the_bytes() {
    let server = MockServer::start().await;
    let wrong = format!("sha256:{}", "b".repeat(64));
    let returned = b"different";
    let canonical = oci_digest(returned);
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/manifests/{wrong}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(returned.to_vec(), MANIFEST_TYPE))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, _, _) = send(&app, Method::GET, &format!("/v2/hub/app/manifests/{wrong}")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(store::get_manifest(&state.serving.meta, &canonical).unwrap().is_none());
    assert!(!store::manifest_is_member(&state.serving.meta, "hub", "app", &canonical).unwrap());
    let (status, _, body) = send(&app, Method::GET, &format!("/v2/hub/app/manifests/{canonical}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&body, "MANIFEST_UNKNOWN"), "{body:?}");
}
#[tokio::test]
async fn test_manifest_by_tag_accepts_a_non_sha256_advertised_digest() {
    let server = MockServer::start().await;
    let body = br#"{"schemaVersion":2,"config":{}}"#;
    // OCI permits advertised algorithms other than the local canonical hash.
    Mock::given(method("GET"))
        .and(path("/v2/app/manifests/latest"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", format!("sha512:{}", "a".repeat(128)).as_str())
                .set_body_raw(body.to_vec(), MANIFEST_TYPE),
        )
        .mount(&server)
        .await;
    mount_head_without_digest(&server, "/v2/app/manifests/latest").await;
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, headers, got) = send(&app, Method::GET, "/v2/hub/app/manifests/latest").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &body[..]);
    assert_eq!(headers["docker-content-digest"], oci_digest(body));
    assert_eq!(
        store::get_tag(&state.serving.meta, "hub", "app", "latest").unwrap(),
        Some(oci_digest(body))
    );
}
#[tokio::test]
async fn test_manifest_by_tag_wrong_sha256_advertised_is_a_gateway_error() {
    let server = MockServer::start().await;
    let body = br#"{"schemaVersion":2,"config":{}}"#;
    // A mismatched canonical digest would poison the cache.
    Mock::given(method("GET"))
        .and(path("/v2/app/manifests/latest"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", format!("sha256:{}", "b".repeat(64)).as_str())
                .set_body_raw(body.to_vec(), MANIFEST_TYPE),
        )
        .mount(&server)
        .await;
    mount_head_without_digest(&server, "/v2/app/manifests/latest").await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, _, _) = send(&app, Method::GET, "/v2/hub/app/manifests/latest").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}
#[rstest]
#[case::short_sha256("sha256:abcd")]
#[case::non_hex_sha256(&format!("sha256:{}", "g".repeat(64)))]
#[case::uppercase_sha256(&format!("sha256:{}", "A".repeat(64)))]
#[case::sha512(&format!("sha512:{}", "c".repeat(128)))]
#[case::unknown_algorithm(&format!("multihash:{}", "d".repeat(64)))]
#[tokio::test]
async fn test_manifest_digest_is_rejected_before_route_effects(#[case] reference: &str) {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    for method in [Method::GET, Method::HEAD, Method::PUT, Method::DELETE] {
        let (status, _, body) = send(&app, method.clone(), &format!("/v2/hub/app/manifests/{reference}")).await;
        assert_eq!(
            (status, body_has_code(&body, "DIGEST_INVALID")),
            (StatusCode::BAD_REQUEST, method != Method::HEAD),
            "method {method}"
        );
    }
    let (status, _, body) = send(&app, Method::GET, &format!("/v2/missing/app/manifests/{reference}")).await;
    assert_eq!(
        (status, body_has_code(&body, "DIGEST_INVALID")),
        (StatusCode::BAD_REQUEST, true)
    );
    assert_eq!(
        (
            server.received_requests().await.unwrap().len(),
            state.serving.meta.current_serial().unwrap()
        ),
        (0, 0)
    );
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
    store::put_tag(&state.serving.meta, "store", "app", "latest", &digest).unwrap();
    store::put_tag(&state.serving.meta, "store", "app", "v2", &digest).unwrap();
    let (status, _, body) = send(&app, Method::GET, "/v2/store/app/tags/list").await;
    assert_eq!(status, StatusCode::OK);
    let text = std::str::from_utf8(&body).unwrap();
    assert!(text.contains("\"store/app\""), "{text}");
    assert!(text.contains("\"latest\"") && text.contains("\"v2\""), "{text}");
}
#[tokio::test]
async fn test_manifest_upstream_unreachable_is_a_gateway_error() {
    let dir = tempfile::tempdir().unwrap();
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
    let descriptor = referrer_descriptor(&sig);
    let index = referrer_index(&[descriptor.clone(), descriptor]);
    Mock::given(method("GET"))
        .and(path(format!("/v2/library/nginx/referrers/{subject}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(index, "application/vnd.oci.image.index.v1+json"))
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
#[tokio::test]
async fn test_referrers_fall_back_to_the_tag_schema_when_the_api_is_absent() {
    let server = MockServer::start().await;
    let subject = format!("sha256:{}", "a".repeat(64));
    let sig = format!("sha256:{}", "b".repeat(64));
    // OCI referrers fall back to the subject-tag schema after a `404`.
    Mock::given(method("GET"))
        .and(path(format!("/v2/library/nginx/referrers/{subject}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let index = referrer_index(&[referrer_descriptor(&sig)]);
    Mock::given(method("GET"))
        .and(path(format!("/v2/library/nginx/manifests/sha256-{}", "a".repeat(64))))
        .respond_with(ResponseTemplate::new(200).set_body_raw(index, "application/vnd.oci.image.index.v1+json"))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let base = format!("/v2/hub/library/nginx/referrers/{subject}");

    let (status, _, body) = send(&app, Method::GET, &base).await;
    assert_eq!(status, StatusCode::OK);
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
}
#[rstest]
#[case::native_unauthorized(false, 401, StatusCode::UNAUTHORIZED, "UNAUTHORIZED", None)]
#[case::native_throttled(false, 429, StatusCode::TOO_MANY_REQUESTS, "TOOMANYREQUESTS", Some("19"))]
#[case::native_server_error(false, 500, StatusCode::BAD_GATEWAY, "UNKNOWN", None)]
#[case::fallback_unauthorized(true, 401, StatusCode::UNAUTHORIZED, "UNAUTHORIZED", None)]
#[case::fallback_throttled(true, 429, StatusCode::TOO_MANY_REQUESTS, "TOOMANYREQUESTS", Some("19"))]
#[case::fallback_server_error(true, 500, StatusCode::BAD_GATEWAY, "UNKNOWN", None)]
#[tokio::test]
async fn test_referrers_preserve_upstream_status(
    #[case] fallback: bool,
    #[case] upstream_status: u16,
    #[case] expected_status: StatusCode,
    #[case] expected_code: &str,
    #[case] expected_retry_after: Option<&str>,
) {
    let server = MockServer::start().await;
    let subject = format!("sha256:{}", "a".repeat(64));
    Mock::given(method("GET"))
        .and(path(format!("/v2/library/nginx/referrers/{subject}")))
        .respond_with(if fallback {
            ResponseTemplate::new(404)
        } else {
            referrer_failure(upstream_status)
        })
        .expect(1)
        .mount(&server)
        .await;
    if fallback {
        Mock::given(method("GET"))
            .and(path(format!("/v2/library/nginx/manifests/sha256-{}", "a".repeat(64))))
            .respond_with(referrer_failure(upstream_status))
            .expect(1)
            .mount(&server)
            .await;
    }
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);

    let (status, headers, body) = send(&app, Method::GET, &format!("/v2/hub/library/nginx/referrers/{subject}")).await;
    assert_eq!(
        (
            status,
            body_has_code(&body, expected_code),
            headers.get(header::RETRY_AFTER).and_then(|value| value.to_str().ok()),
        ),
        (expected_status, true, expected_retry_after)
    );
}

#[rstest]
#[case::content_type(
    r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[]}"#,
    "application/json"
)]
#[case::malformed_content_type(
    r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[]}"#,
    "application/vnd.oci.image.index.v1+json; broken"
)]
#[case::json("{", "application/vnd.oci.image.index.v1+json")]
#[case::document_shape("[]", "application/vnd.oci.image.index.v1+json")]
#[case::schema_version(
    r#"{"schemaVersion":1,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[]}"#,
    "application/vnd.oci.image.index.v1+json"
)]
#[case::body_media_type(
    r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","manifests":[]}"#,
    "application/vnd.oci.image.index.v1+json"
)]
#[case::manifests_shape(
    r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":{}}"#,
    "application/vnd.oci.image.index.v1+json"
)]
#[case::descriptor_shape(
    r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{"digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}]}"#,
    "application/vnd.oci.image.index.v1+json"
)]
#[case::descriptor_not_object(
    r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[null]}"#,
    "application/vnd.oci.image.index.v1+json"
)]
#[case::annotations_shape(
    r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":1,"annotations":[]}]}"#,
    "application/vnd.oci.image.index.v1+json"
)]
#[case::annotation_value(
    r#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","size":1,"annotations":{"key":1}}]}"#,
    "application/vnd.oci.image.index.v1+json"
)]
#[tokio::test]
async fn test_native_referrers_reject_malformed_success(#[case] body: &str, #[case] content_type: &str) {
    let server = MockServer::start().await;
    let subject = format!("sha256:{}", "a".repeat(64));
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/referrers/{subject}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), content_type))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);

    let (status, _, body) = send(&app, Method::GET, &format!("/v2/hub/app/referrers/{subject}")).await;
    assert_eq!(
        (status, body_has_code(&body, "UNKNOWN")),
        (StatusCode::BAD_GATEWAY, true)
    );
}

#[rstest]
#[case::json("{")]
#[case::shape(r#"{"schemaVersion":2,"manifests":{}}"#)]
#[tokio::test]
async fn test_fallback_referrers_reject_malformed_index(#[case] body: &str) {
    let server = MockServer::start().await;
    let subject = format!("sha256:{}", "a".repeat(64));
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/referrers/{subject}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/manifests/sha256-{}", "a".repeat(64))))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(body.as_bytes().to_vec(), "application/vnd.oci.image.index.v1+json"),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);

    let (status, _, body) = send(&app, Method::GET, &format!("/v2/hub/app/referrers/{subject}")).await;
    assert_eq!(
        (status, body_has_code(&body, "UNKNOWN")),
        (StatusCode::BAD_GATEWAY, true)
    );
}

#[tokio::test]
async fn test_referrers_report_cache_write_failure() {
    let server = MockServer::start().await;
    let subject = format!("sha256:{}", "a".repeat(64));
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/referrers/{subject}")))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(referrer_index(&[]), "application/vnd.oci.image.index.v1+json"),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("peryx.redb");
    drop(peryx_storage::meta::MetaStore::open(&database).unwrap());
    let index = crate::tests::oci_index(
        "hub",
        "hub",
        peryx_index::IndexKind::Cached {
            client: peryx_upstream::UpstreamClient::new(&format!("{}/", server.uri())).unwrap(),
            offline: false,
        },
    );
    let mut state = peryx_driver::AppState::with_clock(
        peryx_storage::meta::MetaStore::open_existing_read_only(database).unwrap(),
        peryx_storage::blob::BlobStore::new(dir.path().join("blobs")),
        60,
        vec![index],
        std::sync::Arc::new(|| 1000),
    );
    crate::tests::install_oci(&mut state, std::collections::HashMap::new(), false);
    let app = peryx_http::router(std::sync::Arc::new(state));

    let (status, _, body) = send(&app, Method::GET, &format!("/v2/hub/app/referrers/{subject}")).await;
    assert_eq!(
        (status, body_has_code(&body, "UNKNOWN")),
        (StatusCode::BAD_GATEWAY, true)
    );
}

#[tokio::test]
async fn test_native_empty_referrers_is_a_successful_empty_index() {
    let server = MockServer::start().await;
    let subject = format!("sha256:{}", "a".repeat(64));
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/referrers/{subject}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            referrer_index(&[]),
            "application/vnd.oci.image.index.v1+json; charset=utf-8",
        ))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);

    let (status, _, body) = send(&app, Method::GET, &format!("/v2/hub/app/referrers/{subject}")).await;
    assert_eq!(
        (
            status,
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["manifests"].clone(),
        ),
        (StatusCode::OK, serde_json::json!([]))
    );
}

#[tokio::test]
async fn test_fallback_missing_tag_is_a_successful_empty_index() {
    let server = MockServer::start().await;
    let subject = format!("sha256:{}", "a".repeat(64));
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/referrers/{subject}")))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/manifests/sha256-{}", "a".repeat(64))))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);

    let (status, _, body) = send(&app, Method::GET, &format!("/v2/hub/app/referrers/{subject}")).await;
    assert_eq!(
        (
            status,
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["manifests"].clone(),
        ),
        (StatusCode::OK, serde_json::json!([]))
    );
}

#[tokio::test]
async fn test_fallback_non_index_is_a_successful_empty_index() {
    let server = MockServer::start().await;
    let subject = format!("sha256:{}", "a".repeat(64));
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/referrers/{subject}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/manifests/sha256-{}", "a".repeat(64))))
        .respond_with(ResponseTemplate::new(200).set_body_raw(b"not json".to_vec(), MANIFEST_TYPE))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);

    let (status, _, body) = send(&app, Method::GET, &format!("/v2/hub/app/referrers/{subject}")).await;
    assert_eq!(
        (
            status,
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["manifests"].clone(),
        ),
        (StatusCode::OK, serde_json::json!([]))
    );
}

#[rstest]
#[case::native(false)]
#[case::fallback(true)]
#[tokio::test]
async fn test_referrers_preserve_transport_failure(#[case] fallback: bool) {
    if !fallback {
        let dir = tempfile::tempdir().unwrap();
        let (_state, app) = proxy(&dir, "http://127.0.0.1:1/", false);
        let subject = format!("sha256:{}", "a".repeat(64));
        assert_eq!(
            send(&app, Method::GET, &format!("/v2/hub/app/referrers/{subject}"))
                .await
                .0,
            StatusCode::BAD_GATEWAY
        );
        return;
    }
    let server = MockServer::start().await;
    let subject = format!("sha256:{}", "a".repeat(64));
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/referrers/{subject}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/manifests/sha256-{}", "a".repeat(64))))
        .respond_with(ResponseTemplate::new(307).insert_header("location", "http://127.0.0.1:1/disconnected"))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);

    assert_eq!(
        send(&app, Method::GET, &format!("/v2/hub/app/referrers/{subject}"))
            .await
            .0,
        StatusCode::BAD_GATEWAY
    );
}

#[tokio::test]
async fn test_referrers_keep_a_successful_cache_after_revalidation_fails() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, Ordering};

    let server = MockServer::start().await;
    let subject = format!("sha256:{}", "a".repeat(64));
    let descriptor = referrer_descriptor(&format!("sha256:{}", "b".repeat(64)));
    let upstream_path = format!("/v2/app/referrers/{subject}");
    Mock::given(method("GET"))
        .and(path(upstream_path.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            referrer_index(std::slice::from_ref(&descriptor)),
            "application/vnd.oci.image.index.v1+json",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let now = Arc::new(AtomicI64::new(1000));
    let ticking = Arc::clone(&now);
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = crate::tests::proxy_with_clock(
        &dir,
        &format!("{}/", server.uri()),
        Arc::new(move || ticking.load(Ordering::Relaxed)),
    );
    let uri = format!("/v2/hub/app/referrers/{subject}");
    assert_eq!(send(&app, Method::GET, &uri).await.0, StatusCode::OK);

    server.reset().await;
    Mock::given(method("GET"))
        .and(path(upstream_path))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&server)
        .await;
    now.store(1061, Ordering::Relaxed);
    assert_eq!(send(&app, Method::GET, &uri).await.0, StatusCode::BAD_GATEWAY);

    server.reset().await;
    now.store(1001, Ordering::Relaxed);
    let (status, _, body) = send(&app, Method::GET, &uri).await;
    assert_eq!(
        (
            status,
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["manifests"].clone(),
        ),
        (StatusCode::OK, serde_json::json!([descriptor]))
    );
}
#[tokio::test]
async fn test_manifest_by_digest_does_not_cross_repositories_on_one_proxy() {
    let server = MockServer::start().await;
    let body = br#"{"schemaVersion":2,"config":{}}"#;
    let digest = oci_digest(body);
    for repo in ["alpha", "gamma"] {
        Mock::given(method("GET"))
            .and(path(format!("/v2/{repo}/manifests/{digest}")))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body.to_vec(), MANIFEST_TYPE))
            .mount(&server)
            .await;
    }
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy(&dir, &format!("{}/", server.uri()), false);

    let (status, _, got) = send(&app, Method::GET, &format!("/v2/hub/alpha/manifests/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &body[..]);

    // Shared bytes must not bypass repository membership.
    let (status, _, denied) = send(&app, Method::GET, &format!("/v2/hub/beta/manifests/{digest}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&denied, "MANIFEST_UNKNOWN"), "{denied:?}");
    assert!(!store::manifest_is_member(&state.serving.meta, "hub", "beta", &digest).unwrap());
    let (head, ..) = send(&app, Method::HEAD, &format!("/v2/hub/beta/manifests/{digest}")).await;
    assert_eq!(head, StatusCode::NOT_FOUND);

    let (status, _, got) = send(&app, Method::GET, &format!("/v2/hub/gamma/manifests/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &body[..]);
    assert!(store::manifest_is_member(&state.serving.meta, "hub", "gamma", &digest).unwrap());
}
#[tokio::test]
async fn test_manifest_by_digest_does_not_cross_indexes() {
    let server_a = MockServer::start().await;
    let server_b = MockServer::start().await;
    let body = br#"{"schemaVersion":2,"config":{}}"#;
    let digest = oci_digest(body);
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/manifests/{digest}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.to_vec(), MANIFEST_TYPE))
        .mount(&server_a)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy_pair(&dir, &format!("{}/", server_a.uri()), &format!("{}/", server_b.uri()));

    let (status, _, got) = send(&app, Method::GET, &format!("/v2/hub/app/manifests/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &body[..]);

    // Shared bytes must not bypass index-specific upstream authorization.
    let (status, _, denied) = send(&app, Method::GET, &format!("/v2/vault/app/manifests/{digest}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&denied, "MANIFEST_UNKNOWN"), "{denied:?}");
    assert!(!store::manifest_is_member(&state.serving.meta, "vault", "app", &digest).unwrap());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_concurrent_by_digest_misses_across_indexes_do_not_leak() {
    let server_a = MockServer::start().await;
    let server_b = MockServer::start().await;
    let body = br#"{"schemaVersion":2,"config":{}}"#;
    let digest = oci_digest(body);
    let (gate, response) = gated_response(ResponseTemplate::new(200).set_body_raw(body.to_vec(), MANIFEST_TYPE));
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/manifests/{digest}")))
        .respond_with(response)
        .mount(&server_a)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy_pair(&dir, &format!("{}/", server_a.uri()), &format!("{}/", server_b.uri()));
    let uri_a = format!("/v2/hub/app/manifests/{digest}");
    let uri_b = format!("/v2/vault/app/manifests/{digest}");
    let from_a = tokio::spawn({
        let app = app.clone();
        async move { send(&app, Method::GET, &uri_a).await }
    });
    let release = gate.entered().await;
    let (from_b, from_b_pending) = observe_pending({
        let app = app.clone();
        async move { send(&app, Method::GET, &uri_b).await }
    });
    from_b_pending.await.unwrap();
    drop(release);
    let (from_a, from_b) = (from_a.await.unwrap(), from_b.await.unwrap());
    assert_eq!(from_a.0, StatusCode::OK);
    assert_eq!(from_a.2, &body[..]);
    assert_eq!(from_b.0, StatusCode::NOT_FOUND);
    assert!(body_has_code(&from_b.2, "MANIFEST_UNKNOWN"), "{:?}", from_b.2);
    assert!(!store::manifest_is_member(&state.serving.meta, "vault", "app", &digest).unwrap());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_concurrent_by_digest_misses_share_one_upstream_fetch() {
    let server = MockServer::start().await;
    let body = br#"{"schemaVersion":2,"config":{}}"#;
    let digest = oci_digest(body);
    let (gate, response) = gated_response(ResponseTemplate::new(200).set_body_raw(body.to_vec(), MANIFEST_TYPE));
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/manifests/{digest}")))
        .respond_with(response)
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let uri = format!("/v2/hub/app/manifests/{digest}");
    let first = tokio::spawn({
        let app = app.clone();
        let uri = uri.clone();
        async move { send(&app, Method::GET, &uri).await }
    });
    let release = gate.entered().await;
    let (second, second_pending) = observe_pending({
        let app = app.clone();
        let uri = uri.clone();
        async move { send(&app, Method::GET, &uri).await }
    });
    second_pending.await.unwrap();
    drop(release);
    let (first, second) = (first.await.unwrap(), second.await.unwrap());
    assert_eq!(first.0, StatusCode::OK);
    assert_eq!(second.0, StatusCode::OK);
    assert_eq!(first.2, &body[..]);
    assert_eq!(second.2, &body[..]);
}
#[tokio::test]
async fn test_by_digest_member_with_evicted_bytes_refetches() {
    let server = MockServer::start().await;
    let body = br#"{"schemaVersion":2,"config":{}}"#;
    let digest = oci_digest(body);
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/manifests/{digest}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.to_vec(), MANIFEST_TYPE))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    // Membership without bytes must refetch from the owning upstream.
    store::record_manifest(
        &state.serving.meta,
        "hub",
        "app",
        "sha256:index",
        &Manifest {
            media_type: "application/vnd.oci.image.index.v1+json".to_owned(),
            bytes: format!(r#"{{"manifests":[{{"digest":"{digest}"}}]}}"#).into_bytes(),
        },
    )
    .unwrap();

    let (status, _, got) = send(&app, Method::GET, &format!("/v2/hub/app/manifests/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &body[..]);
}

fn referrer_descriptor(digest: &str) -> serde_json::Value {
    serde_json::json!({
        "mediaType": MANIFEST_TYPE,
        "digest": digest,
        "size": 1,
        "artifactType": "application/vnd.example.sig",
    })
}

fn referrer_index(manifests: &[serde_json::Value]) -> Vec<u8> {
    serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": manifests,
    })
    .to_string()
    .into_bytes()
}

fn referrer_failure(status: u16) -> ResponseTemplate {
    let response = ResponseTemplate::new(status);
    if status == 429 {
        response.insert_header("retry-after", "19")
    } else {
        response
    }
}
