//! The virtual OCI role: member walking, hosted-shadows-upstream, aggregation, and upload routing.

use axum::http::{Method, StatusCode};
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{app_with_indexes, auth, oci_digest, oci_index, send, send_body, virtual_stack, writable_index};

const TOKEN: &str = "s3cret";
const MANIFEST_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

/// Push a manifest to the virtual index under `tag`; it lands in the hosted member.
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

async fn wait_for_upstream(server: &MockServer, wanted: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|request| request.url.path() == wanted)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
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

    // The virtual pull returns the hosted manifest, not upstream's, the dependency-confusion defense.
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
    // No hosted image for `edge`, so the proxy member answers.
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

#[tokio::test]
async fn test_virtual_digest_delete_wins_an_inflight_proxy_pull() {
    let server = MockServer::start().await;
    let manifest = br#"{"schemaVersion":2,"from":"upstream"}"#;
    let digest = oci_digest(manifest);
    let upstream_path = format!("/v2/app/manifests/{digest}");
    Mock::given(method("GET"))
        .and(path(upstream_path.clone()))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(manifest.to_vec(), MANIFEST_TYPE)
                .set_delay(std::time::Duration::from_millis(300)),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = virtual_stack(&dir, &format!("{}/", server.uri()));
    let pull_app = app.clone();
    let pull_uri = format!("/v2/reg/app/manifests/{digest}");
    let pull = tokio::spawn(async move { send(&pull_app, Method::GET, &pull_uri).await });
    wait_for_upstream(&server, &upstream_path).await;
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

    assert_eq!(pull.await.unwrap().0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_virtual_tag_delete_wins_an_inflight_proxy_pull() {
    let server = MockServer::start().await;
    let manifest = br#"{"schemaVersion":2,"from":"upstream"}"#;
    let digest = oci_digest(manifest);
    let upstream_path = "/v2/app/manifests/latest";
    Mock::given(method("HEAD"))
        .and(path(upstream_path))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", digest.as_str())
                .set_delay(std::time::Duration::from_millis(300)),
        )
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
    wait_for_upstream(&server, upstream_path).await;
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

#[tokio::test]
async fn test_virtual_subject_delete_wins_inflight_referrer_discovery() {
    let server = MockServer::start().await;
    let manifest = br#"{"schemaVersion":2,"kind":"subject"}"#;
    let subject = oci_digest(manifest);
    let referrer = format!("sha256:{}", "a".repeat(64));
    let upstream_path = format!("/v2/app/referrers/{subject}");
    Mock::given(method("GET"))
        .and(path(upstream_path.clone()))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(
                    serde_json::json!({
                        "schemaVersion": 2,
                        "manifests": [{"digest": referrer}],
                    })
                    .to_string()
                    .into_bytes(),
                    "application/vnd.oci.image.index.v1+json",
                )
                .set_delay(std::time::Duration::from_millis(300)),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = virtual_stack(&dir, &format!("{}/", server.uri()));
    push_to_virtual(&app, "base", manifest).await;
    let pull_app = app.clone();
    let pull_uri = format!("/v2/reg/app/referrers/{subject}");
    let pull = tokio::spawn(async move { send(&pull_app, Method::GET, &pull_uri).await });
    wait_for_upstream(&server, &upstream_path).await;
    let delete = send_body(
        &app,
        Method::DELETE,
        &format!("/v2/reg/app/manifests/{subject}"),
        &[("authorization", &auth(TOKEN))],
        Vec::new(),
    )
    .await;
    assert_eq!(delete.0, StatusCode::ACCEPTED);

    let (status, _, body) = pull.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    let index: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(index["manifests"].as_array().unwrap().is_empty());
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
                    upload: Some(0),
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
    // Page one answers a request with no cursor; page two answers the `last` cursor its Link points to.
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
    // A Link without a rel="next" marks the last page, so aggregation stops here.
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
    // The proxy upstream refuses every connection; the union still returns the hosted tag.
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
                    upload: None,
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
    // A misconfiguration the config layer would reject, but the resolver must still decline safely.
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
                    upload: Some(0),
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
