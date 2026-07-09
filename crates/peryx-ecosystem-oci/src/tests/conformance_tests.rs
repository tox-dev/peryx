//! Distribution-spec conformance details: referrers + `OCI-Subject`, tag pagination, upload status,
//! and chunk-contiguity `416`s, the paths the OCI conformance suite exercises.

use axum::http::{Method, StatusCode, header};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{auth, hosted_writable, oci_digest, proxy, send, send_body, send_with};

const TOKEN: &str = "s3cret";
const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

#[tokio::test]
async fn test_manifest_with_subject_records_a_referrer_and_echoes_it() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let subject = format!("sha256:{}", "5".repeat(64));
    let manifest = format!(
        r#"{{"schemaVersion":2,"mediaType":"{MANIFEST_TYPE}","artifactType":"application/vnd.example+type","subject":{{"mediaType":"{MANIFEST_TYPE}","digest":"{subject}","size":7}}}}"#
    );
    let digest = oci_digest(manifest.as_bytes());

    let (status, headers, _) = send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/v1",
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        manifest.clone().into_bytes(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(headers["oci-subject"], subject);

    // The referrers API lists the manifest that named the subject.
    let (status, headers, body) = send(&app, Method::GET, &format!("/v2/store/app/referrers/{subject}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "application/vnd.oci.image.index.v1+json");
    let index: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let manifests = index["manifests"].as_array().unwrap();
    assert_eq!(manifests.len(), 1);
    assert_eq!(manifests[0]["digest"], digest);
    assert_eq!(manifests[0]["artifactType"], "application/vnd.example+type");
}

#[tokio::test]
async fn test_referrers_for_an_unknown_subject_is_an_empty_index() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let subject = format!("sha256:{}", "6".repeat(64));
    let (status, headers, body) = send(&app, Method::GET, &format!("/v2/store/app/referrers/{subject}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "application/vnd.oci.image.index.v1+json");
    let index: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(index["manifests"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_referrers_on_an_unresolvable_name_is_name_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, "http://127.0.0.1:1/", false);
    let subject = format!("sha256:{}", "7".repeat(64));
    let (status, _, body) = send(&app, Method::GET, &format!("/v2/other/app/referrers/{subject}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(super::body_has_code(&body, "NAME_UNKNOWN"), "{body:?}");
}

#[tokio::test]
async fn test_manifest_without_a_subject_has_no_referrer_header() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let (_status, headers, _) = send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/plain",
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        br#"{"schemaVersion":2}"#.to_vec(),
    )
    .await;
    assert!(!headers.contains_key("oci-subject"));
}

#[tokio::test]
async fn test_upload_status_reports_progress() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let (_status, headers, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    let location = headers[header::LOCATION].to_str().unwrap().to_owned();

    send_body(
        &app,
        Method::PATCH,
        &location,
        &[("authorization", &auth(TOKEN))],
        b"0123456789".to_vec(),
    )
    .await;

    let (status, headers, _) = send_with(&app, Method::GET, &location, &[("authorization", &auth(TOKEN))]).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(headers[header::RANGE], "0-9");
    assert!(headers.contains_key("docker-upload-uuid"));
}

#[tokio::test]
async fn test_referrer_artifact_type_falls_back_to_config_and_keeps_annotations() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let subject = format!("sha256:{}", "8".repeat(64));
    // No top-level artifactType, so the config media type is used; annotations carry through.
    let manifest = format!(
        r#"{{"schemaVersion":2,"config":{{"mediaType":"application/vnd.example.config"}},"annotations":{{"key":"value"}},"subject":{{"digest":"{subject}"}}}}"#
    );
    send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/cfg",
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        manifest.into_bytes(),
    )
    .await;
    let (_status, _, body) = send(&app, Method::GET, &format!("/v2/store/app/referrers/{subject}")).await;
    let index: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let descriptor = &index["manifests"].as_array().unwrap()[0];
    assert_eq!(descriptor["artifactType"], "application/vnd.example.config");
    assert_eq!(descriptor["annotations"]["key"], "value");
}

#[tokio::test]
async fn test_non_json_manifest_records_no_referrer() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let (status, headers, _) = send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/raw",
        &[("authorization", &auth(TOKEN))],
        b"not json at all".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(!headers.contains_key("oci-subject"));
}

#[tokio::test]
async fn test_upload_status_on_a_read_only_proxy_is_denied() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, "http://127.0.0.1:1/", false);
    let (status, _, _) = send(&app, Method::GET, "/v2/hub/app/blobs/uploads/whatever").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_upload_status_for_an_unknown_session_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let (status, _, body) = send_with(
        &app,
        Method::GET,
        "/v2/store/app/blobs/uploads/deadbeef",
        &[("authorization", &auth(TOKEN))],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(super::body_has_code(&body, "BLOB_UPLOAD_UNKNOWN"), "{body:?}");
}

#[tokio::test]
async fn test_contiguous_chunk_with_content_range_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let (_status, headers, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    let location = headers[header::LOCATION].to_str().unwrap().to_owned();
    let (status, _, _) = send_body(
        &app,
        Method::PATCH,
        &location,
        &[("authorization", &auth(TOKEN)), ("content-range", "0-4")],
        b"hello".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn test_out_of_order_chunk_on_patch_is_range_not_satisfiable() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let (_status, headers, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    let location = headers[header::LOCATION].to_str().unwrap().to_owned();
    // Start at byte 5 while the session is empty, out of order.
    let (status, headers, _) = send_body(
        &app,
        Method::PATCH,
        &location,
        &[("authorization", &auth(TOKEN)), ("content-range", "5-9")],
        b"world".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert!(headers.contains_key(header::RANGE));
}

#[tokio::test]
async fn test_out_of_order_chunk_on_put_is_range_not_satisfiable() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    let (_status, headers, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    let location = headers[header::LOCATION].to_str().unwrap().to_owned();
    let (status, _, _) = send_body(
        &app,
        Method::PUT,
        &format!("{location}?digest=sha256:x"),
        &[("authorization", &auth(TOKEN)), ("content-range", "5-9")],
        b"world".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
}

#[tokio::test]
async fn test_tag_list_pagination() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    for tag in ["a", "b", "c", "d"] {
        send_body(
            &app,
            Method::PUT,
            &format!("/v2/store/app/manifests/{tag}"),
            &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
            br#"{"schemaVersion":2}"#.to_vec(),
        )
        .await;
    }

    // First page of two, with a Link to the next.
    let (status, headers, body) = send(&app, Method::GET, "/v2/store/app/tags/list?n=2").await;
    assert_eq!(status, StatusCode::OK);
    let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(page["tags"], serde_json::json!(["a", "b"]));
    assert!(headers[header::LINK].to_str().unwrap().contains("last=b"));

    // `last` returns the tags after the cursor.
    let (_status, _, body) = send(&app, Method::GET, "/v2/store/app/tags/list?last=b").await;
    let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(page["tags"], serde_json::json!(["c", "d"]));
}

#[tokio::test]
async fn test_proxy_tag_list_forwards_the_pagination_query() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/app/tags/list"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(br#"{"name":"app","tags":["only"]}"#.to_vec(), "application/json"),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, _, body) = send(&app, Method::GET, "/v2/hub/app/tags/list?n=5&last=x").await;
    assert_eq!(status, StatusCode::OK);
    assert!(std::str::from_utf8(&body).unwrap().contains("\"only\""));
}
