//! The virtual OCI role: member walking, hosted-shadows-upstream, aggregation, and upload routing.

use axum::http::{Method, StatusCode};
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{app_with_indexes, auth, oci_digest, oci_index, send, send_body, virtual_stack};

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
    let (status, _, got) = send(&app, Method::GET, &format!("/v2/reg/app/blobs/{digest}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, &blob[..]);
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
    use velodex_http::IndexKind;
    let dir = tempfile::tempdir().unwrap();
    let (_state, app) = app_with_indexes(
        &dir,
        vec![
            oci_index(
                "images",
                "images",
                IndexKind::Hosted {
                    upload_token: Some(TOKEN.to_owned()),
                    volatile: true,
                },
            ),
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
    use velodex_http::IndexKind;
    use velodex_upstream::UpstreamClient;
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
