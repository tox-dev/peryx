//! Digest revocations on OCI content and discovery routes.

use std::str::FromStr as _;

use axum::http::{Method, StatusCode, header};
use peryx_core::UiProjectView;
use peryx_driver::AppState;
use peryx_identity::{ArtifactDigest, RevocationReason, UserId};
use rstest::rstest;
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{body_has_code, hosted, oci_digest, proxy, send, send_with, virtual_stack};
use crate::store::{self, Manifest};

const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const INDEX_TYPE: &str = "application/vnd.oci.image.index.v1+json";
const LEGACY_ACCEPT: &str = "application/vnd.docker.distribution.manifest.v2+json";

fn revoke(state: &AppState, digest: &str) {
    state
        .revocations
        .put(
            &ArtifactDigest::from_str(digest).unwrap(),
            &RevocationReason::new("compromised builder").unwrap(),
            &UserId::random(),
            1_000,
        )
        .unwrap();
}

fn lift(state: &AppState, digest: &str) {
    state
        .revocations
        .lift(&ArtifactDigest::from_str(digest).unwrap(), &UserId::random(), 1_001)
        .unwrap();
}

fn store_manifest(state: &AppState, repo: &str, tag: &str, body: &[u8]) -> String {
    let digest = oci_digest(body);
    store::record_manifest(
        &state.meta,
        "store",
        repo,
        &digest,
        &Manifest {
            media_type: MANIFEST_TYPE.to_owned(),
            bytes: body.to_vec(),
        },
    )
    .unwrap();
    store::put_tag(&state.meta, "store", repo, tag, &digest).unwrap();
    digest
}

#[rstest]
#[case::digest_get(Method::GET, true)]
#[case::digest_head(Method::HEAD, true)]
#[case::tag_get(Method::GET, false)]
#[case::tag_head(Method::HEAD, false)]
#[tokio::test]
async fn test_revoked_manifest_is_unknown_through_every_reference(#[case] method: Method, #[case] by_digest: bool) {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted(&dir);
    let body = br#"{"schemaVersion":2}"#;
    let digest = store_manifest(&state, "app", "latest", body);
    revoke(&state, &digest);
    let reference = if by_digest { digest.as_str() } else { "latest" };

    let (status, headers, response) = send(&app, method, &format!("/v2/store/app/manifests/{reference}")).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert!(response.is_empty() || body_has_code(&response, "MANIFEST_UNKNOWN"));
    assert!(!String::from_utf8_lossy(&response).contains("compromised builder"));
}

#[tokio::test]
async fn test_lift_restores_a_stored_manifest_with_bounded_cache_headers() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted(&dir);
    let body = br#"{"schemaVersion":2}"#;
    let digest = store_manifest(&state, "app", "latest", body);
    revoke(&state, &digest);
    lift(&state, &digest);
    let uri = "/v2/store/app/manifests/latest";

    let (status, headers, response) = send(&app, Method::GET, uri).await;
    assert_eq!(
        (
            status,
            headers[header::CACHE_CONTROL].to_str().unwrap(),
            response.as_ref()
        ),
        (
            StatusCode::OK,
            "public, max-age=60, must-revalidate, no-transform",
            body.as_slice(),
        )
    );

    let (status, headers, response) = send_with(&app, Method::GET, uri, &[("authorization", "Basic dW51c2Vk")]).await;
    assert_eq!(
        (
            status,
            headers[header::CACHE_CONTROL].to_str().unwrap(),
            response.as_ref()
        ),
        (
            StatusCode::OK,
            "private, max-age=60, must-revalidate, no-transform",
            body.as_slice(),
        )
    );
}

#[tokio::test]
async fn test_lift_does_not_restore_deleted_manifest_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted(&dir);
    let digest = store_manifest(&state, "app", "latest", br#"{"schemaVersion":2}"#);
    revoke(&state, &digest);
    store::delete_manifest(&state.meta, &digest).unwrap();
    lift(&state, &digest);

    let (status, _, response) = send(&app, Method::GET, "/v2/store/app/manifests/latest").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&response, "MANIFEST_UNKNOWN"), "{response:?}");
}

#[rstest]
#[case::get(Method::GET, "", None)]
#[case::head(Method::HEAD, "", None)]
#[case::range(Method::GET, "", Some("bytes=0-3"))]
#[case::contents(Method::GET, "/contents", None)]
#[tokio::test]
async fn test_revoked_blob_is_unknown_on_each_content_route(
    #[case] method: Method,
    #[case] suffix: &str,
    #[case] range: Option<&str>,
) {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted(&dir);
    let digest = format!(
        "sha256:{}",
        state.blobs.put_bytes(b"shared layer").await.unwrap().as_str()
    );
    store::record_blob_membership(&state.meta, "store", "app", &digest).unwrap();
    revoke(&state, &digest);
    let uri = format!("/v2/store/app/blobs/{digest}{suffix}");
    let headers = range.map_or_else(Vec::new, |value| vec![("range", value)]);

    let (status, response_headers, response) = send_with(&app, method, &uri, &headers).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(response_headers[header::CACHE_CONTROL], "no-store");
    assert!(response.is_empty() || body_has_code(&response, "BLOB_UNKNOWN"));
}

#[tokio::test]
async fn test_revoked_shared_blob_is_hidden_under_each_repository() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted(&dir);
    let bytes = b"shared config";
    let digest = format!("sha256:{}", state.blobs.put_bytes(bytes).await.unwrap().as_str());
    for repo in ["first", "second"] {
        store::record_blob_membership(&state.meta, "store", repo, &digest).unwrap();
    }
    revoke(&state, &digest);

    for repo in ["first", "second"] {
        let (status, _, response) = send(&app, Method::GET, &format!("/v2/store/{repo}/blobs/{digest}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{repo}");
        assert!(body_has_code(&response, "BLOB_UNKNOWN"), "{repo}: {response:?}");
    }
}

#[tokio::test]
async fn test_lift_restores_a_blob_range_with_the_cache_bound() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted(&dir);
    let bytes = b"shared config";
    let digest = format!("sha256:{}", state.blobs.put_bytes(bytes).await.unwrap().as_str());
    store::record_blob_membership(&state.meta, "store", "app", &digest).unwrap();
    revoke(&state, &digest);
    lift(&state, &digest);

    let (status, headers, response) = send_with(
        &app,
        Method::GET,
        &format!("/v2/store/app/blobs/{digest}"),
        &[("range", "bytes=0-5")],
    )
    .await;

    assert_eq!(
        (
            status,
            headers[header::CACHE_CONTROL].to_str().unwrap(),
            response.as_ref()
        ),
        (
            StatusCode::PARTIAL_CONTENT,
            "public, max-age=60, must-revalidate, no-transform",
            &bytes[..6],
        )
    );
}

#[tokio::test]
async fn test_revoked_manifest_digest_never_reaches_the_proxy() {
    let server = MockServer::start().await;
    let digest = format!("sha256:{}", "a".repeat(64));
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/manifests/{digest}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(b"revoked".to_vec(), MANIFEST_TYPE))
        .expect(0)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    revoke(&state, &digest);

    let (status, _, _) = send(&app, Method::GET, &format!("/v2/hub/app/manifests/{digest}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_revoked_proxy_tag_target_stops_after_head() {
    let server = MockServer::start().await;
    let digest = format!("sha256:{}", "b".repeat(64));
    Mock::given(method("HEAD"))
        .and(path("/v2/app/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).insert_header("docker-content-digest", digest.as_str()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/app/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(b"revoked".to_vec(), MANIFEST_TYPE))
        .expect(0)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    revoke(&state, &digest);

    let (status, _, response) = send(&app, Method::GET, "/v2/hub/app/manifests/latest").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&response, "MANIFEST_UNKNOWN"), "{response:?}");
}

#[tokio::test]
async fn test_revoked_canonical_manifest_is_not_cached_when_head_has_no_digest() {
    let server = MockServer::start().await;
    let body = br#"{"schemaVersion":2}"#;
    let digest = oci_digest(body);
    Mock::given(method("HEAD"))
        .and(path("/v2/app/manifests/latest"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/app/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.to_vec(), MANIFEST_TYPE))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    revoke(&state, &digest);

    let (status, _, _) = send(&app, Method::GET, "/v2/hub/app/manifests/latest").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(store::get_manifest(&state.meta, &digest).unwrap(), None);
}

#[tokio::test]
async fn test_revoked_canonical_manifest_blocks_a_non_sha256_digest_pull() {
    let server = MockServer::start().await;
    let body = br#"{"schemaVersion":2}"#;
    let canonical = oci_digest(body);
    let requested = format!("sha512:{}", "c".repeat(128));
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/manifests/{requested}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.to_vec(), MANIFEST_TYPE))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    revoke(&state, &canonical);

    let (status, _, response) = send(&app, Method::GET, &format!("/v2/hub/app/manifests/{requested}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&response, "MANIFEST_UNKNOWN"), "{response:?}");
    assert_eq!(store::get_manifest(&state.meta, &canonical).unwrap(), None);
}

#[rstest]
#[case::fresh(999)]
#[case::stale_after_upstream_failure(900)]
#[tokio::test]
async fn test_revoked_proxy_tag_never_serves_from_cache(#[case] fetched_at: i64) {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy(&dir, "http://127.0.0.1:1/", false);
    let body = br#"{"schemaVersion":2}"#;
    let digest = oci_digest(body);
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
    store::put_tag(&state.meta, "hub", "app", "latest", &digest).unwrap();
    store::set_tag_freshness(&state.meta, "hub", "app", "latest", &digest, fetched_at).unwrap();
    revoke(&state, &digest);

    let (status, _, response) = send(&app, Method::GET, "/v2/hub/app/manifests/latest").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body_has_code(&response, "MANIFEST_UNKNOWN"), "{response:?}");
}

#[tokio::test]
async fn test_legacy_manifest_negotiation_cannot_select_a_revoked_child() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted(&dir);
    let child = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#;
    let child_digest = store_manifest(&state, "app", "child", child);
    let index = format!(
        r#"{{"schemaVersion":2,"mediaType":"{INDEX_TYPE}","manifests":[{{"digest":"{child_digest}","platform":{{"os":"linux","architecture":"amd64"}}}}]}}"#,
    )
    .into_bytes();
    let index_digest = oci_digest(&index);
    store::record_manifest(
        &state.meta,
        "store",
        "app",
        &index_digest,
        &Manifest {
            media_type: INDEX_TYPE.to_owned(),
            bytes: index.clone(),
        },
    )
    .unwrap();
    store::put_tag(&state.meta, "store", "app", "multi", &index_digest).unwrap();
    revoke(&state, &child_digest);

    let (modern, _, body) = send(&app, Method::GET, "/v2/store/app/manifests/multi").await;
    assert_eq!((modern, body.as_ref()), (StatusCode::OK, index.as_slice()));
    let (legacy, _, response) = send_with(
        &app,
        Method::GET,
        "/v2/store/app/manifests/multi",
        &[("accept", LEGACY_ACCEPT)],
    )
    .await;
    assert_eq!(legacy, StatusCode::NOT_FOUND);
    assert!(body_has_code(&response, "MANIFEST_UNKNOWN"), "{response:?}");
}

#[tokio::test]
async fn test_hosted_tag_filter_runs_before_pagination() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted(&dir);
    let clear = format!("sha256:{}", "1".repeat(64));
    let revoked = format!("sha256:{}", "2".repeat(64));
    for (tag, digest) in [("a-clear", &clear), ("b-revoked", &revoked), ("c-clear", &clear)] {
        store::put_tag(&state.meta, "store", "app", tag, digest).unwrap();
    }
    revoke(&state, &revoked);

    let (status, headers, body) = send(&app, Method::GET, "/v2/store/app/tags/list?n=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"name":"store/app","tags":["a-clear"]})
    );
    assert_eq!(
        headers[header::LINK],
        "</v2/store/app/tags/list?n=1&last=a-clear>; rel=\"next\""
    );

    let (status, headers, body) = send(&app, Method::GET, "/v2/store/app/tags/list?n=1&last=a-clear").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"name":"store/app","tags":["c-clear"]})
    );
    assert!(!headers.contains_key(header::LINK));
}

#[tokio::test]
async fn test_proxy_tag_filter_resolves_and_caches_targets() {
    let server = MockServer::start().await;
    let clear = format!("sha256:{}", "3".repeat(64));
    let revoked = format!("sha256:{}", "4".repeat(64));
    Mock::given(method("GET"))
        .and(path("/v2/app/tags/list"))
        .and(query_param_is_missing("last"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("link", "</v2/app/tags/list?n=2&last=revoked>; rel=\"next\"")
                .set_body_raw(
                    br#"{"name":"app","tags":["clear","revoked"]}"#.to_vec(),
                    "application/json",
                ),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/app/tags/list"))
        .and(query_param("last", "revoked"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(br#"{"name":"app","tags":[]}"#.to_vec(), "application/json"),
        )
        .expect(1)
        .mount(&server)
        .await;
    for (tag, digest) in [("clear", &clear), ("revoked", &revoked)] {
        Mock::given(method("HEAD"))
            .and(path(format!("/v2/app/manifests/{tag}")))
            .respond_with(ResponseTemplate::new(200).insert_header("docker-content-digest", digest.as_str()))
            .expect(1)
            .mount(&server)
            .await;
    }
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    revoke(&state, &revoked);

    for _ in 0..2 {
        let (status, headers, body) = send(&app, Method::GET, "/v2/hub/app/tags/list").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers[header::LINK],
            "</v2/hub/app/tags/list?n=2&last=revoked>; rel=\"next\""
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({"name":"hub/app","tags":["clear"]})
        );
    }

    let driver = state.driver_for(crate::ECOSYSTEM).unwrap().clone();
    let view = driver
        .capabilities()
        .browse
        .unwrap()
        .browse_project(state.serving.clone(), 0, "app".to_owned())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        view,
        UiProjectView::References {
            names: vec!["clear".to_owned()]
        }
    );
}

#[tokio::test]
async fn test_active_proxy_tag_filter_preserves_an_upstream_failure() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/app/tags/list"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    revoke(&state, &format!("sha256:{}", "8".repeat(64)));

    let (status, headers, _) = send(&app, Method::GET, "/v2/hub/app/tags/list").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
}

#[rstest]
#[case::invalid_json(br"{")]
#[case::invalid_tags_type(br#"{"name":"app","tags":"latest"}"#)]
#[tokio::test]
async fn test_active_proxy_tag_filter_rejects_an_invalid_document(#[case] body: &[u8]) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/app/tags/list"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.to_vec(), "application/json"))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    revoke(&state, &format!("sha256:{}", "8".repeat(64)));

    let (status, _, _) = send(&app, Method::GET, "/v2/hub/app/tags/list").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn test_active_proxy_tag_filter_accepts_a_null_tag_list() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/app/tags/list"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(br#"{"name":"app","tags":null}"#.to_vec(), "application/json"),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    revoke(&state, &format!("sha256:{}", "8".repeat(64)));

    let (status, _, body) = send(&app, Method::GET, "/v2/hub/app/tags/list").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"name":"hub/app","tags":[]})
    );
}

#[tokio::test]
async fn test_active_proxy_tag_filter_omits_an_unresolved_tag() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/app/tags/list"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(br#"{"name":"app","tags":["missing"]}"#.to_vec(), "application/json"),
        )
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .and(path("/v2/app/manifests/missing"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    revoke(&state, &format!("sha256:{}", "8".repeat(64)));

    let (status, _, body) = send(&app, Method::GET, "/v2/hub/app/tags/list").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"name":"hub/app","tags":[]})
    );
}

#[tokio::test]
async fn test_virtual_tag_union_ignores_an_invalid_proxy_page() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/app/tags/list"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(b"not json".to_vec(), "application/json"))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = virtual_stack(&dir, &format!("{}/", server.uri()));

    let (status, _, body) = send(&app, Method::GET, "/v2/reg/app/tags/list").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
        serde_json::json!({"name":"reg/app","tags":[]})
    );
}

#[tokio::test]
async fn test_referrers_hide_revoked_descriptors_and_subjects() {
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = hosted(&dir);
    let subject = format!("sha256:{}", "5".repeat(64));
    let clear = format!("sha256:{}", "6".repeat(64));
    let revoked = format!("sha256:{}", "7".repeat(64));
    store::put_referrer(
        &state.meta,
        "store",
        "app",
        &subject,
        &format!("sha256:{}", "8".repeat(64)),
        br#"{"artifactType":"application/vnd.example.sig"}"#,
    )
    .unwrap();
    store::put_referrer(
        &state.meta,
        "store",
        "app",
        &subject,
        &format!("sha256:{}", "9".repeat(64)),
        b"not json",
    )
    .unwrap();
    for digest in [&clear, &revoked] {
        store::put_referrer(
            &state.meta,
            "store",
            "app",
            &subject,
            digest,
            serde_json::json!({"digest":digest,"artifactType":"application/vnd.example.sig"})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
    }
    revoke(&state, &revoked);
    let uri = format!("/v2/store/app/referrers/{subject}?artifactType=application/vnd.example.sig");

    let (status, headers, body) = send(&app, Method::GET, &uri).await;
    let document = serde_json::from_slice::<serde_json::Value>(&body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["oci-filters-applied"], "artifactType");
    assert_eq!(
        document["manifests"],
        serde_json::json!([{"digest":clear,"artifactType":"application/vnd.example.sig"}])
    );

    revoke(&state, &subject);
    let (status, _, body) = send(&app, Method::GET, &uri).await;
    let document = serde_json::from_slice::<serde_json::Value>(&body).unwrap();
    assert_eq!(
        (status, &document["manifests"]),
        (StatusCode::OK, &serde_json::json!([]))
    );
}
