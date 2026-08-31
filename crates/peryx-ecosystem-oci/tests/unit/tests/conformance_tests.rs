use axum::http::{Method, StatusCode, header};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{
    CONFIG_BLOB, auth, hosted_writable, image_manifest, oci_digest, proxy, seed_config, send, send_body, send_with,
};

const TOKEN: &str = "s3cret";
const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

async fn push_subject_and_referrer(app: &axum::Router) -> (String, String) {
    seed_config(app, "store/app", &auth(TOKEN)).await;
    let subject_manifest = image_manifest(MANIFEST_TYPE, "");
    let subject = oci_digest(&subject_manifest);
    let status = send_body(
        app,
        Method::PUT,
        "/v2/store/app/manifests/base",
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        subject_manifest,
    )
    .await
    .0;
    assert_eq!(status, StatusCode::CREATED);
    let manifest = image_manifest(
        MANIFEST_TYPE,
        &format!(r#","subject":{{"mediaType":"{MANIFEST_TYPE}","digest":"{subject}","size":7}}"#),
    );
    let digest = oci_digest(&manifest);
    let status = send_body(
        app,
        Method::PUT,
        "/v2/store/app/manifests/sig",
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        manifest,
    )
    .await
    .0;
    assert_eq!(status, StatusCode::CREATED);
    (subject, digest)
}

async fn referrer_count(app: &axum::Router, subject: &str) -> usize {
    let (status, _, body) = send(app, Method::GET, &format!("/v2/store/app/referrers/{subject}")).await;
    assert_eq!(status, StatusCode::OK);
    serde_json::from_slice::<serde_json::Value>(&body).unwrap()["manifests"]
        .as_array()
        .unwrap()
        .len()
}

#[tokio::test]
async fn test_manifest_with_subject_records_a_referrer_and_echoes_it() {
    let dir = tempfile::tempdir().unwrap();
    let (_, app) = hosted_writable(&dir, TOKEN);
    seed_config(&app, "store/app", &auth(TOKEN)).await;
    let subject = format!("sha256:{}", "5".repeat(64));
    let manifest = image_manifest(
        MANIFEST_TYPE,
        &format!(
            r#","artifactType":"application/vnd.example+type","subject":{{"mediaType":"{MANIFEST_TYPE}","digest":"{subject}","size":7}}"#
        ),
    );
    let digest = oci_digest(&manifest);

    let (status, headers, _) = send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/v1",
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        manifest.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(headers["oci-subject"], subject);

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
async fn test_referrer_discovery_follows_referrer_trash_and_restore() {
    let dir = tempfile::tempdir().unwrap();
    let (_, app) = hosted_writable(&dir, TOKEN);
    let (subject, digest) = push_subject_and_referrer(&app).await;
    let status = send_body(
        &app,
        Method::DELETE,
        &format!("/v2/store/app/manifests/{digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await
    .0;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(referrer_count(&app, &subject).await, 0);

    let status = send_body(
        &app,
        Method::PUT,
        &format!("/v2/store/app/manifests/{digest}/restore"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await
    .0;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(referrer_count(&app, &subject).await, 1);
}

#[tokio::test]
async fn test_referrer_discovery_follows_subject_trash_and_restore() {
    let dir = tempfile::tempdir().unwrap();
    let (_, app) = hosted_writable(&dir, TOKEN);
    let (subject, _) = push_subject_and_referrer(&app).await;
    let status = send_body(
        &app,
        Method::DELETE,
        &format!("/v2/store/app/manifests/{subject}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await
    .0;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(referrer_count(&app, &subject).await, 0);

    let status = send_body(
        &app,
        Method::PUT,
        &format!("/v2/store/app/manifests/{subject}/restore"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await
    .0;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(referrer_count(&app, &subject).await, 1);
}

#[tokio::test]
async fn test_referrers_for_an_unknown_subject_is_an_empty_index() {
    let dir = tempfile::tempdir().unwrap();
    let (_, app) = hosted_writable(&dir, TOKEN);
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
    let (_, app) = proxy(&dir, "http://127.0.0.1:1/", false);
    let subject = format!("sha256:{}", "7".repeat(64));
    let (status, _, body) = send(&app, Method::GET, &format!("/v2/other/app/referrers/{subject}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(super::body_has_code(&body, "NAME_UNKNOWN"), "{body:?}");
}

#[tokio::test]
async fn test_manifest_without_a_subject_has_no_referrer_header() {
    let dir = tempfile::tempdir().unwrap();
    let (_, app) = hosted_writable(&dir, TOKEN);
    seed_config(&app, "store/app", &auth(TOKEN)).await;
    let (status, headers, _) = send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/plain",
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        image_manifest(MANIFEST_TYPE, ""),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(!headers.contains_key("oci-subject"));
}

#[tokio::test]
async fn test_upload_status_reports_progress() {
    let dir = tempfile::tempdir().unwrap();
    let (_, app) = hosted_writable(&dir, TOKEN);
    let (status, headers, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let location = headers[header::LOCATION].to_str().unwrap().to_owned();

    let status = send_body(
        &app,
        Method::PATCH,
        &location,
        &[("authorization", &auth(TOKEN))],
        b"0123456789".to_vec(),
    )
    .await
    .0;
    assert_eq!(status, StatusCode::ACCEPTED);

    let (status, headers, _) = send_with(&app, Method::GET, &location, &[("authorization", &auth(TOKEN))]).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(headers[header::RANGE], "0-9");
    assert!(headers.contains_key("docker-upload-uuid"));
}

#[tokio::test]
async fn test_referrer_artifact_type_falls_back_to_config_and_keeps_annotations() {
    let dir = tempfile::tempdir().unwrap();
    let (_, app) = hosted_writable(&dir, TOKEN);
    seed_config(&app, "store/app", &auth(TOKEN)).await;
    let subject = format!("sha256:{}", "8".repeat(64));
    // The referrer descriptor falls back to the config media type when no artifactType is declared.
    let manifest = format!(
        r#"{{"schemaVersion":2,"mediaType":"{MANIFEST_TYPE}","config":{{"mediaType":"application/vnd.example.config","digest":"{}","size":{}}},"layers":[],"annotations":{{"key":"value"}},"subject":{{"mediaType":"{MANIFEST_TYPE}","digest":"{subject}","size":7}}}}"#,
        oci_digest(CONFIG_BLOB),
        CONFIG_BLOB.len(),
    )
    .into_bytes();
    let status = send_body(
        &app,
        Method::PUT,
        "/v2/store/app/manifests/cfg",
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        manifest,
    )
    .await
    .0;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _, body) = send(&app, Method::GET, &format!("/v2/store/app/referrers/{subject}")).await;
    assert_eq!(status, StatusCode::OK);
    let index: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let descriptor = &index["manifests"].as_array().unwrap()[0];
    assert_eq!(descriptor["artifactType"], "application/vnd.example.config");
    assert_eq!(descriptor["annotations"]["key"], "value");
}

#[tokio::test]
async fn test_upload_status_on_a_read_only_proxy_is_denied() {
    let dir = tempfile::tempdir().unwrap();
    let (_, app) = proxy(&dir, "http://127.0.0.1:1/", false);
    let (status, _, _) = send(&app, Method::GET, "/v2/hub/app/blobs/uploads/whatever").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_upload_status_for_an_unknown_session_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (_, app) = hosted_writable(&dir, TOKEN);
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
    let (_, app) = hosted_writable(&dir, TOKEN);
    let (status, headers, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
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
    let (_, app) = hosted_writable(&dir, TOKEN);
    let (status, headers, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let location = headers[header::LOCATION].to_str().unwrap().to_owned();
    let (status, headers, _) = send_body(
        &app,
        Method::PATCH,
        &location,
        &[("authorization", &auth(TOKEN)), ("content-range", "5-9")],
        b"world".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(headers[header::RANGE], "0-0");
    assert_eq!(headers[header::LOCATION].to_str().unwrap(), location);
    assert!(headers.contains_key("docker-upload-uuid"));

    let (status, headers, _) = send_with(&app, Method::GET, &location, &[("authorization", &auth(TOKEN))]).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(headers[header::RANGE], "0-0");

    let digest = oci_digest(b"");
    let (status, _, _) = send_body(
        &app,
        Method::PUT,
        &format!("{location}?digest={digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _, body) = send(&app, Method::GET, &format!("/v2/store/app/blobs/{digest}")).await;
    assert_eq!((status, body.as_ref()), (StatusCode::OK, b"".as_slice()));
}

#[tokio::test]
async fn test_unreadable_content_range_on_patch_is_range_not_satisfiable() {
    let dir = tempfile::tempdir().unwrap();
    let (_, app) = hosted_writable(&dir, TOKEN);
    let (status, headers, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let location = headers[header::LOCATION].to_str().unwrap().to_owned();
    let (status, headers, _) = send_body(
        &app,
        Method::PATCH,
        &location,
        &[("authorization", &auth(TOKEN)), ("content-range", "somewhere")],
        b"world".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(headers[header::RANGE], "0-0");
}

#[tokio::test]
async fn test_content_range_wider_than_the_body_is_range_not_satisfiable() {
    let dir = tempfile::tempdir().unwrap();
    let (_, app) = hosted_writable(&dir, TOKEN);
    let (status, headers, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let location = headers[header::LOCATION].to_str().unwrap().to_owned();
    let (status, headers, _) = send_body(
        &app,
        Method::PATCH,
        &location,
        &[("authorization", &auth(TOKEN)), ("content-range", "0-999")],
        b"x".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(headers[header::RANGE], "0-0");
    assert_eq!(headers[header::LOCATION].to_str().unwrap(), location);
    let (status, headers, _) = send_with(&app, Method::GET, &location, &[("authorization", &auth(TOKEN))]).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(headers[header::RANGE], "0-0");
}

#[tokio::test]
async fn test_empty_session_reports_a_zero_range() {
    let dir = tempfile::tempdir().unwrap();
    let (_, app) = hosted_writable(&dir, TOKEN);
    let (status, headers, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(headers[header::RANGE], "0-0");

    let location = headers[header::LOCATION].to_str().unwrap().to_owned();
    let (status, headers, _) = send_with(&app, Method::GET, &location, &[("authorization", &auth(TOKEN))]).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(headers[header::RANGE], "0-0");
}

#[tokio::test]
async fn test_out_of_order_chunk_on_put_is_range_not_satisfiable() {
    let dir = tempfile::tempdir().unwrap();
    let (_, app) = hosted_writable(&dir, TOKEN);
    let (status, headers, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let location = headers[header::LOCATION].to_str().unwrap().to_owned();
    let (status, headers, _) = send_body(
        &app,
        Method::PUT,
        &format!("{location}?digest=sha256:x"),
        &[("authorization", &auth(TOKEN)), ("content-range", "5-9")],
        b"world".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(headers[header::LOCATION].to_str().unwrap(), location);
    assert!(headers.contains_key("docker-upload-uuid"));
}

#[tokio::test]
async fn test_referrers_with_a_malformed_digest_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (_, app) = hosted_writable(&dir, TOKEN);
    for reference in ["sha256:bad", "not-a-digest"] {
        let (status, _, body) = send(&app, Method::GET, &format!("/v2/store/app/referrers/{reference}")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{reference}");
        assert!(super::body_has_code(&body, "DIGEST_INVALID"), "{body:?}");
    }
}

#[tokio::test]
async fn test_cancel_upload_session_returns_no_content() {
    let dir = tempfile::tempdir().unwrap();
    let (_, app) = hosted_writable(&dir, TOKEN);
    let (status, headers, _) = send_body(
        &app,
        Method::POST,
        "/v2/store/app/blobs/uploads/",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let location = headers[header::LOCATION].to_str().unwrap().to_owned();

    let (status, _, _) = send_with(&app, Method::DELETE, &location, &[("authorization", &auth(TOKEN))]).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, _, _) = send_with(&app, Method::GET, &location, &[("authorization", &auth(TOKEN))]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_cancel_unknown_upload_session_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let (_, app) = hosted_writable(&dir, TOKEN);
    let (status, _, body) = send_with(
        &app,
        Method::DELETE,
        "/v2/store/app/blobs/uploads/deadbeef",
        &[("authorization", &auth(TOKEN))],
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(super::body_has_code(&body, "BLOB_UPLOAD_UNKNOWN"), "{body:?}");
}

#[tokio::test]
async fn test_cancel_upload_on_a_read_only_proxy_is_denied() {
    let dir = tempfile::tempdir().unwrap();
    let (_, app) = proxy(&dir, "http://127.0.0.1:1/", false);
    let (status, _, _) = send(&app, Method::DELETE, "/v2/hub/app/blobs/uploads/whatever").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_tag_list_pagination() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = hosted_writable(&dir, TOKEN);
    seed_config(&app, "store/app", &auth(TOKEN)).await;
    for tag in ["a", "b", "c", "d"] {
        let status = send_body(
            &app,
            Method::PUT,
            &format!("/v2/store/app/manifests/{tag}"),
            &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
            image_manifest(MANIFEST_TYPE, ""),
        )
        .await
        .0;
        assert_eq!(status, StatusCode::CREATED, "{tag}");
    }

    let (status, headers, body) = send(&app, Method::GET, "/v2/store/app/tags/list?n=2").await;
    assert_eq!(status, StatusCode::OK);
    let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(page["tags"], serde_json::json!(["a", "b"]));
    assert!(headers[header::LINK].to_str().unwrap().contains("last=b"));

    let (status, _, body) = send(&app, Method::GET, "/v2/store/app/tags/list?last=b").await;
    assert_eq!(status, StatusCode::OK);
    let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(page["tags"], serde_json::json!(["c", "d"]));

    let (status, headers, body) = send(&app, Method::GET, "/v2/store/app/tags/list?n=0").await;
    assert_eq!(status, StatusCode::OK);
    let page: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(page["tags"], serde_json::json!([]));
    assert!(!headers.contains_key(header::LINK));
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

#[tokio::test]
async fn test_proxy_tag_list_rewrites_the_next_link_to_the_client_facing_name() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/app/tags/list"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("link", "</v2/app/tags/list?last=z&n=1>; rel=\"next\"")
                .set_body_raw(br#"{"name":"app","tags":["only"]}"#.to_vec(), "application/json"),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, headers, _) = send(&app, Method::GET, "/v2/hub/app/tags/list?n=1").await;
    assert_eq!(status, StatusCode::OK);
    let link = headers[header::LINK].to_str().unwrap();
    assert!(link.contains("</v2/hub/app/tags/list?last=z&n=1>"), "{link}");
    assert!(link.contains("rel=\"next\""), "{link}");
}

#[tokio::test]
async fn test_proxy_tag_list_skips_a_prev_member_to_rewrite_the_next_link() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/app/tags/list"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header(
                    "link",
                    "</v2/app/tags/list?last=a>; rel=\"prev\", </v2/app/tags/list?last=z>; rel=\"next\"",
                )
                .set_body_raw(br#"{"name":"app","tags":["m"]}"#.to_vec(), "application/json"),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, headers, _) = send(&app, Method::GET, "/v2/hub/app/tags/list").await;
    assert_eq!(status, StatusCode::OK);
    let link = headers[header::LINK].to_str().unwrap();
    assert!(link.contains("</v2/hub/app/tags/list?last=z>"), "{link}");
    assert!(!link.contains("last=a"), "{link}");
}

#[tokio::test]
async fn test_proxy_tag_list_keeps_a_comma_in_the_next_cursor() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/app/tags/list"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("link", "</v2/app/tags/list?last=z&token=a,b>; rel=\"next\"")
                .set_body_raw(br#"{"name":"app","tags":["m"]}"#.to_vec(), "application/json"),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = proxy(&dir, &format!("{}/", server.uri()), false);
    let (status, headers, _) = send(&app, Method::GET, "/v2/hub/app/tags/list").await;
    assert_eq!(status, StatusCode::OK);
    let link = headers[header::LINK].to_str().unwrap();
    assert!(link.contains("</v2/hub/app/tags/list?last=z&token=a,b>"), "{link}");
    assert!(link.contains("rel=\"next\""), "{link}");
}
