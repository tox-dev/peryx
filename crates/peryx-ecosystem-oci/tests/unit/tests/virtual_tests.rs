use axum::http::{Method, StatusCode};
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{
    app_with_indexes, auth, gated_response, oci_digest, oci_index, send, send_body, virtual_stack, writable_index,
};

const TOKEN: &str = "s3cret";
const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

async fn push_to_virtual(app: &axum::Router, tag: &str, manifest: &[u8]) {
    let (status, _, _) = send_body(
        app,
        Method::PUT,
        &format!("/v2/reg/app/manifests/{tag}"),
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        manifest.to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn test_virtual_hosted_manifest_shadows_upstream() {
    let server = MockServer::start().await;
    let upstream = br#"{"schemaVersion":2,"from":"upstream"}"#;
    Mock::given(method("GET"))
        .and(path("/v2/app/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(upstream.to_vec(), MANIFEST_TYPE))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = virtual_stack(&dir, &format!("{}/", server.uri()));

    let hosted = br#"{"schemaVersion":2,"from":"hosted"}"#;
    push_to_virtual(&app, "latest", hosted).await;

    let (status, headers, got) = send(&app, Method::GET, "/v2/reg/app/manifests/latest").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &hosted[..]);
    assert_eq!(headers["docker-content-digest"], oci_digest(hosted));
}

#[tokio::test]
async fn test_virtual_falls_through_to_upstream() {
    let server = MockServer::start().await;
    let upstream = br#"{"schemaVersion":2,"from":"upstream"}"#;
    Mock::given(method("GET"))
        .and(path("/v2/app/manifests/edge"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(upstream.to_vec(), MANIFEST_TYPE))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = virtual_stack(&dir, &format!("{}/", server.uri()));
    let (status, _, got) = send(&app, Method::GET, "/v2/reg/app/manifests/edge").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &upstream[..]);
}

#[tokio::test]
async fn test_virtual_manifest_trash_blocks_proxy_fallback_and_tag_discovery() {
    let server = MockServer::start().await;
    let manifest = br#"{"schemaVersion":2,"from":"shared"}"#;
    Mock::given(method("GET"))
        .and(path("/v2/app/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(manifest.to_vec(), MANIFEST_TYPE))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/app/tags/list"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(br#"{"name":"app","tags":["latest"]}"#.to_vec(), "application/json"),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = virtual_stack(&dir, &format!("{}/", server.uri()));
    push_to_virtual(&app, "latest", manifest).await;
    let digest = oci_digest(manifest);
    let (status, _, _) = send_body(
        &app,
        Method::DELETE,
        &format!("/v2/reg/app/manifests/{digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    for reference in ["latest", &digest] {
        assert_eq!(
            send(&app, Method::GET, &format!("/v2/reg/app/manifests/{reference}"))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }
    let (status, _, body) = send(&app, Method::GET, "/v2/reg/app/tags/list").await;
    assert_eq!(status, StatusCode::OK);
    assert!(!std::str::from_utf8(&body).unwrap().contains("latest"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_virtual_digest_delete_wins_an_inflight_proxy_pull() {
    let server = MockServer::start().await;
    let manifest = br#"{"schemaVersion":2,"from":"upstream"}"#;
    let digest = oci_digest(manifest);
    let upstream_path = format!("/v2/app/manifests/{digest}");
    let (gate, response) = gated_response(ResponseTemplate::new(200).set_body_raw(manifest.to_vec(), MANIFEST_TYPE));
    Mock::given(method("GET"))
        .and(path(upstream_path.clone()))
        .respond_with(response)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = virtual_stack(&dir, &format!("{}/", server.uri()));
    let pull_app = app.clone();
    let pull_uri = format!("/v2/reg/app/manifests/{digest}");
    let pull = tokio::spawn(async move { send(&pull_app, Method::GET, &pull_uri).await });
    let release = gate.entered().await;
    let push = send_body(
        &app,
        Method::PUT,
        &format!("/v2/reg/app/manifests/{digest}"),
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        manifest.to_vec(),
    )
    .await;
    assert_eq!(push.0, StatusCode::CREATED);
    let delete = send_body(
        &app,
        Method::DELETE,
        &format!("/v2/reg/app/manifests/{digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(delete.0, StatusCode::ACCEPTED);

    drop(release);
    assert_eq!(pull.await.unwrap().0, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_virtual_tag_delete_wins_an_inflight_proxy_pull() {
    let server = MockServer::start().await;
    let manifest = br#"{"schemaVersion":2,"from":"upstream"}"#;
    let digest = oci_digest(manifest);
    let upstream_path = "/v2/app/manifests/latest";
    let (gate, response) =
        gated_response(ResponseTemplate::new(200).insert_header("docker-content-digest", digest.as_str()));
    Mock::given(method("HEAD"))
        .and(path(upstream_path))
        .respond_with(response)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(upstream_path))
        .respond_with(ResponseTemplate::new(200).set_body_raw(manifest.to_vec(), MANIFEST_TYPE))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = virtual_stack(&dir, &format!("{}/", server.uri()));
    let pull_app = app.clone();
    let pull = tokio::spawn(async move { send(&pull_app, Method::GET, "/v2/reg/app/manifests/latest").await });
    let release = gate.entered().await;
    push_to_virtual(&app, "latest", manifest).await;
    let delete = send_body(
        &app,
        Method::DELETE,
        "/v2/reg/app/manifests/latest",
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(delete.0, StatusCode::ACCEPTED);

    drop(release);
    assert_eq!(pull.await.unwrap().0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_virtual_digest_tombstone_blocks_a_lower_proxy_tag() {
    let server = MockServer::start().await;
    let manifest = br#"{"schemaVersion":2,"from":"shared"}"#;
    let digest = oci_digest(manifest);
    Mock::given(method("HEAD"))
        .and(path("/v2/app/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).insert_header("docker-content-digest", digest.as_str()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/app/manifests/latest"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(manifest.to_vec(), MANIFEST_TYPE))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = virtual_stack(&dir, &format!("{}/", server.uri()));
    let push = send_body(
        &app,
        Method::PUT,
        &format!("/v2/reg/app/manifests/{digest}"),
        &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
        manifest.to_vec(),
    )
    .await;
    assert_eq!(push.0, StatusCode::CREATED);
    let delete = send_body(
        &app,
        Method::DELETE,
        &format!("/v2/reg/app/manifests/{digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(delete.0, StatusCode::ACCEPTED);

    assert_eq!(
        send(&app, Method::GET, "/v2/reg/app/manifests/latest").await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_virtual_subject_delete_wins_inflight_referrer_discovery() {
    let server = MockServer::start().await;
    let manifest = br#"{"schemaVersion":2,"kind":"subject"}"#;
    let subject = oci_digest(manifest);
    let referrer = format!("sha256:{}", "a".repeat(64));
    let upstream_path = format!("/v2/app/referrers/{subject}");
    let (gate, response) = gated_response(
        ResponseTemplate::new(200).set_body_raw(
            serde_json::json!({
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.index.v1+json",
                "manifests": [{
                    "mediaType": MANIFEST_TYPE,
                    "digest": referrer,
                    "size": 1,
                }],
            })
            .to_string()
            .into_bytes(),
            "application/vnd.oci.image.index.v1+json",
        ),
    );
    Mock::given(method("GET"))
        .and(path(upstream_path.clone()))
        .respond_with(response)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = virtual_stack(&dir, &format!("{}/", server.uri()));
    push_to_virtual(&app, "base", manifest).await;
    let pull_app = app.clone();
    let pull_uri = format!("/v2/reg/app/referrers/{subject}");
    let pull = tokio::spawn(async move { send(&pull_app, Method::GET, &pull_uri).await });
    let release = gate.entered().await;
    let delete = send_body(
        &app,
        Method::DELETE,
        &format!("/v2/reg/app/manifests/{subject}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(delete.0, StatusCode::ACCEPTED);

    drop(release);
    let (status, _, body) = pull.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    let index: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(index["manifests"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_virtual_referrers_discard_local_results_when_a_proxy_fails() {
    let server = MockServer::start().await;
    let subject = format!("sha256:{}", "b".repeat(64));
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/referrers/{subject}")))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = virtual_stack(&dir, &format!("{}/", server.uri()));
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": MANIFEST_TYPE,
        "artifactType": "application/vnd.example.sig",
        "subject": {"digest": subject},
    })
    .to_string();
    push_to_virtual(&app, "signature", manifest.as_bytes()).await;

    assert_eq!(
        send(&app, Method::GET, &format!("/v2/reg/app/referrers/{subject}"))
            .await
            .0,
        StatusCode::BAD_GATEWAY
    );
}

#[tokio::test]
async fn test_virtual_referrers_keep_local_results_when_a_proxy_is_empty() {
    let server = MockServer::start().await;
    let subject = format!("sha256:{}", "b".repeat(64));
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/referrers/{subject}")))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(
                serde_json::json!({
                    "schemaVersion": 2,
                    "mediaType": "application/vnd.oci.image.index.v1+json",
                    "manifests": [],
                })
                .to_string()
                .into_bytes(),
                "application/vnd.oci.image.index.v1+json",
            ),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = virtual_stack(&dir, &format!("{}/", server.uri()));
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": MANIFEST_TYPE,
        "artifactType": "application/vnd.example.sig",
        "subject": {"digest": subject},
    })
    .to_string();
    let referrer = oci_digest(manifest.as_bytes());
    push_to_virtual(&app, "signature", manifest.as_bytes()).await;

    let (status, _, body) = send(&app, Method::GET, &format!("/v2/reg/app/referrers/{subject}")).await;
    assert_eq!(
        (
            status,
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["manifests"][0]["digest"].clone(),
        ),
        (StatusCode::OK, serde_json::json!(referrer))
    );
}

#[tokio::test]
async fn test_virtual_lower_tombstone_does_not_hide_a_higher_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = app_with_indexes(
        &dir,
        vec![
            writable_index("high", "high", true, TOKEN),
            writable_index("low", "low", true, TOKEN),
            oci_index(
                "reg",
                "reg",
                peryx_index::IndexKind::Virtual {
                    layers: vec![0, 1],
                    write_target: Some(0),
                },
            ),
        ],
    );
    let bytes = br#"{"schemaVersion":2,"from":"high"}"#;
    let digest = oci_digest(bytes);
    for index in ["high", "low"] {
        let (status, _, _) = send_body(
            &app,
            Method::PUT,
            &format!("/v2/{index}/app/manifests/latest"),
            &[("authorization", &auth(TOKEN)), ("content-type", MANIFEST_TYPE)],
            bytes.to_vec(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }
    let (status, _, _) = send_body(
        &app,
        Method::DELETE,
        &format!("/v2/low/app/manifests/{digest}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);

    for reference in ["latest", &digest] {
        let (status, _, got) = send(&app, Method::GET, &format!("/v2/reg/app/manifests/{reference}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(got, &bytes[..]);
    }
    let (status, _, body) = send(&app, Method::GET, "/v2/reg/app/tags/list").await;
    assert_eq!(status, StatusCode::OK);
    assert!(std::str::from_utf8(&body).unwrap().contains("latest"));
}

#[tokio::test]
async fn test_virtual_manifest_unknown_when_no_member_has_it() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/app/manifests/absent"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = virtual_stack(&dir, &format!("{}/", server.uri()));
    let (status, _, body) = send(&app, Method::GET, "/v2/reg/app/manifests/absent").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(super::body_has_code(&body, "MANIFEST_UNKNOWN"), "{body:?}");
}

#[tokio::test]
async fn test_virtual_manifest_by_digest_from_proxy_member() {
    let server = MockServer::start().await;
    let manifest = br#"{"schemaVersion":2,"config":{}}"#;
    let digest = oci_digest(manifest);
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/manifests/{digest}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(manifest.to_vec(), MANIFEST_TYPE))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = virtual_stack(&dir, &format!("{}/", server.uri()));
    let (status, _, got) = send(&app, Method::GET, &format!("/v2/reg/app/manifests/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &manifest[..]);
}

#[tokio::test]
async fn test_virtual_blob_from_proxy_member() {
    let server = MockServer::start().await;
    let blob = b"virtual-layer-bytes";
    let digest = oci_digest(blob);
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/blobs/{digest}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(blob.to_vec(), "application/octet-stream"))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = virtual_stack(&dir, &format!("{}/", server.uri()));
    let (first_status, _, first) = send(&app, Method::GET, &format!("/v2/reg/app/blobs/{digest}")).await;
    let (second_status, _, second) = send(&app, Method::GET, &format!("/v2/reg/app/blobs/{digest}")).await;
    assert_eq!(
        (first_status, first.as_ref(), second_status, second.as_ref()),
        (StatusCode::OK, blob.as_slice(), StatusCode::OK, blob.as_slice())
    );
}

#[tokio::test]
async fn test_virtual_blob_does_not_reuse_another_repository_link() {
    let server = MockServer::start().await;
    let blob = b"repository-layer";
    let digest = oci_digest(blob);
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/blobs/{digest}")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(blob.to_vec(), "application/octet-stream"))
        .mount(&server)
        .await;
    Mock::given(method("HEAD"))
        .and(path(format!("/v2/other/blobs/{digest}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = virtual_stack(&dir, &format!("{}/", server.uri()));

    let first_status = send(&app, Method::GET, &format!("/v2/reg/app/blobs/{digest}")).await.0;
    let (second_status, _, body) = send(&app, Method::GET, &format!("/v2/reg/other/blobs/{digest}")).await;

    assert_eq!(
        (first_status, second_status, super::body_has_code(&body, "BLOB_UNKNOWN")),
        (StatusCode::OK, StatusCode::NOT_FOUND, true),
        "{body:?}"
    );
}

#[tokio::test]
async fn test_virtual_blob_unknown_when_absent_everywhere() {
    let server = MockServer::start().await;
    let digest = oci_digest(b"missing");
    Mock::given(method("GET"))
        .and(path(format!("/v2/app/blobs/{digest}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = virtual_stack(&dir, &format!("{}/", server.uri()));
    let (status, _, body) = send(&app, Method::GET, &format!("/v2/reg/app/blobs/{digest}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(super::body_has_code(&body, "BLOB_UNKNOWN"), "{body:?}");
}

#[tokio::test]
async fn test_virtual_tags_union_hosted_and_upstream() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/app/tags/list"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            br#"{"name":"app","tags":["upstream-a","upstream-b"]}"#.to_vec(),
            "application/json",
        ))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = virtual_stack(&dir, &format!("{}/", server.uri()));
    push_to_virtual(&app, "hosted-tag", br#"{"schemaVersion":2}"#).await;

    let (status, _, body) = send(&app, Method::GET, "/v2/reg/app/tags/list").await;
    assert_eq!(status, StatusCode::OK);
    let text = std::str::from_utf8(&body).unwrap();
    for tag in ["hosted-tag", "upstream-a", "upstream-b"] {
        assert!(text.contains(&format!("\"{tag}\"")), "{tag} missing from {text}");
    }
}

#[tokio::test]
async fn test_virtual_tags_follow_upstream_pagination() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v2/app/tags/list"))
        .and(query_param_is_missing("last"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("link", "</v2/app/tags/list?last=upstream-b>; rel=\"next\"")
                .set_body_raw(
                    br#"{"name":"app","tags":["upstream-a","upstream-b"]}"#.to_vec(),
                    "application/json",
                ),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v2/app/tags/list"))
        .and(query_param("last", "upstream-b"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("link", "</v2/app/tags/list?last=upstream-a>; rel=\"prev\"")
                .set_body_raw(br#"{"name":"app","tags":["upstream-c"]}"#.to_vec(), "application/json"),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = virtual_stack(&dir, &format!("{}/", server.uri()));

    let (status, _, body) = send(&app, Method::GET, "/v2/reg/app/tags/list").await;
    assert_eq!(status, StatusCode::OK);
    let text = std::str::from_utf8(&body).unwrap();
    for tag in ["upstream-a", "upstream-b", "upstream-c"] {
        assert!(text.contains(&format!("\"{tag}\"")), "{tag} missing from {text}");
    }
}

#[tokio::test]
async fn test_virtual_tags_tolerate_an_unreachable_proxy() {
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = virtual_stack(&dir, "http://127.0.0.1:1/");
    push_to_virtual(&app, "only-hosted", br#"{"schemaVersion":2}"#).await;
    let (status, _, body) = send(&app, Method::GET, "/v2/reg/app/tags/list").await;
    assert_eq!(status, StatusCode::OK);
    assert!(std::str::from_utf8(&body).unwrap().contains("\"only-hosted\""));
}

#[tokio::test]
async fn test_push_to_virtual_with_no_upload_target_is_read_only() {
    use peryx_index::IndexKind;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = app_with_indexes(
        &dir,
        vec![
            writable_index("images", "images", true, TOKEN),
            oci_index(
                "reg",
                "reg",
                IndexKind::Virtual {
                    layers: vec![0],
                    write_target: None,
                },
            ),
        ],
    );
    let (status, _, body) = send_body(
        &app,
        Method::PUT,
        "/v2/reg/app/manifests/v1",
        &[("authorization", &auth(TOKEN))],
        b"{}".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(super::body_has_code(&body, "DENIED"), "{body:?}");
}

#[tokio::test]
async fn test_push_to_virtual_whose_upload_target_is_a_proxy_is_denied() {
    use peryx_index::IndexKind;
    use peryx_upstream::UpstreamClient;
    // The resolver remains safe when called without config validation.
    let dir = tempfile::tempdir().unwrap();
    let client = UpstreamClient::new("http://127.0.0.1:1/").unwrap();
    let (_state, app) = app_with_indexes(
        &dir,
        vec![
            oci_index("hub", "hub", IndexKind::Cached { client, offline: false }),
            oci_index(
                "reg",
                "reg",
                IndexKind::Virtual {
                    layers: vec![0],
                    write_target: Some(0),
                },
            ),
        ],
    );
    let (status, _, body) = send_body(
        &app,
        Method::PUT,
        "/v2/reg/app/manifests/v1",
        &[("authorization", &auth(TOKEN))],
        b"{}".to_vec(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(super::body_has_code(&body, "DENIED"), "{body:?}");
}
