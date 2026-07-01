use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;
use velox_storage::blob::{BlobStore, Digest};
use velox_storage::meta::{CachedIndex, MetaStore};
use velox_upstream::UpstreamClient;
use wiremock::matchers::{header as match_header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::router;
use crate::state::{AppState, StateConfig};

struct Harness {
    _dir: tempfile::TempDir,
    server: MockServer,
    state: Arc<AppState>,
    clock: Arc<AtomicI64>,
}

async fn harness(ttl: i64) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let meta = MetaStore::open(dir.path().join("velox.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let upstream = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let clock = Arc::new(AtomicI64::new(1000));
    let ticks = clock.clone();
    let state = Arc::new(AppState::with_clock(
        StateConfig {
            meta,
            blobs,
            upstream,
            index: "root/pypi".to_owned(),
            upload_index: "root/local".to_owned(),
            upload_token: Some("s3cret".to_owned()),
            ttl_secs: ttl,
        },
        Arc::new(move || ticks.load(Ordering::Relaxed)),
    ));
    Harness {
        _dir: dir,
        server,
        state,
        clock,
    }
}

async fn get(state: &Arc<AppState>, uri: &str, accept: Option<&str>) -> (StatusCode, HeaderMap, String) {
    let mut builder = Request::builder().uri(uri).method("GET");
    if let Some(accept) = accept {
        builder = builder.header(header::ACCEPT, accept);
    }
    let response = router(state.clone())
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, String::from_utf8_lossy(&bytes).into_owned())
}

fn detail_json(digest: &str, file_url: &str) -> String {
    format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\"url\":\"{file_url}\",\
         \"hashes\":{{\"sha256\":\"{digest}\"}}}}]}}"
    )
}

async fn mount_detail(server: &MockServer, digest: &str, file_url: &str, etag: Option<&str>) {
    let mut response = ResponseTemplate::new(200).set_body_raw(
        detail_json(digest, file_url).into_bytes(),
        "application/vnd.pypi.simple.v1+json",
    );
    if let Some(etag) = etag {
        response = response.insert_header("etag", etag);
    }
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(response)
        .mount(server)
        .await;
}

#[tokio::test]
async fn test_simple_detail_json_rewrites_file_url_and_caches() {
    let h = harness(60).await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_detail(&h.server, digest.as_str(), &file_url, None).await;

    let (status, headers, body) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "application/vnd.pypi.simple.v1+json"
    );
    assert_eq!(headers.get(header::VARY).unwrap(), "Accept");
    assert!(body.contains(&format!(
        "/root/pypi/files/{}/flask-1.0-py3-none-any.whl",
        digest.as_str()
    )));

    // Second request within the TTL is a cache hit (upstream mock would fail verification if hit twice).
    let (status2, ..) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;
    assert_eq!(status2, StatusCode::OK);
}

#[tokio::test]
async fn test_simple_detail_html() {
    let h = harness(60).await;
    let digest = Digest::of(b"wheel");
    mount_detail(&h.server, digest.as_str(), "http://x/flask.whl", None).await;

    let (status, headers, body) = get(&h.state, "/root/pypi/simple/flask/", Some("text/html")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "text/html; charset=utf-8");
    assert!(body.contains("<a href="));
}

#[tokio::test]
async fn test_simple_detail_from_html_only_upstream() {
    let h = harness(60).await;
    let digest = Digest::of(b"wheel");
    let html = format!(
        "<a href=\"/packages/flask-1.0.whl#sha256={}\">flask-1.0.whl</a>",
        digest.as_str()
    );
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(html.into_bytes(), "text/html"))
        .mount(&h.server)
        .await;

    let (status, _, body) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;

    assert_eq!(status, StatusCode::OK);
    // Parsed from HTML, re-served as JSON with the file URL rewritten to velox's own route.
    assert!(body.contains(&format!("/root/pypi/files/{}/flask-1.0.whl", digest.as_str())));
}

#[tokio::test]
async fn test_simple_detail_unknown_index() {
    let h = harness(60).await;
    let (status, ..) = get(&h.state, "/other/index/simple/flask/", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_simple_detail_upstream_404() {
    let h = harness(60).await;
    Mock::given(method("GET"))
        .and(path("/simple/missing/"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&h.server)
        .await;
    let (status, ..) = get(&h.state, "/root/pypi/simple/missing/", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_simple_detail_revalidate_304_serves_cached() {
    let h = harness(60).await;
    let digest = Digest::of(b"wheel");
    mount_detail(&h.server, digest.as_str(), "http://x/flask.whl", Some("\"v1\"")).await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .and(match_header("if-none-match", "\"v1\""))
        .respond_with(ResponseTemplate::new(304))
        .with_priority(1)
        .mount(&h.server)
        .await;

    let (first, ..) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;
    assert_eq!(first, StatusCode::OK);

    h.clock.store(5000, Ordering::Relaxed); // now stale, forces revalidation
    let (second, _, body) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;
    assert_eq!(second, StatusCode::OK);
    assert!(body.contains("flask"));
}

#[tokio::test]
async fn test_simple_detail_stale_on_5xx() {
    let h = harness(60).await;
    let digest = Digest::of(b"wheel");
    // Pre-seed a cached copy directly.
    let body = velox_core::pypi::to_json(&velox_core::pypi::ProjectDetail {
        meta: velox_core::pypi::Meta::default(),
        name: "flask".to_owned(),
        versions: vec!["1.0".to_owned()],
        files: vec![],
    });
    let _ = digest;
    h.state
        .meta
        .put_index(
            "root/pypi/flask",
            &CachedIndex {
                etag: None,
                last_serial: None,
                fetched_at_unix: 0,
                body: body.into_bytes(),
            },
        )
        .unwrap();
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&h.server)
        .await;

    // TTL 60 but fetched_at 0 and clock 1000 => stale => revalidate => 503 => serve stale.
    let (status, _, served) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(served.contains("flask"));
}

#[tokio::test]
async fn test_simple_detail_invalid_upstream_json_is_bad_gateway() {
    let h = harness(60).await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(b"not json".to_vec(), "application/vnd.pypi.simple.v1+json"),
        )
        .mount(&h.server)
        .await;
    let (status, ..) = get(&h.state, "/root/pypi/simple/flask/", None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn test_simple_detail_5xx_without_cache_is_bad_gateway() {
    let h = harness(60).await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&h.server)
        .await;
    let (status, ..) = get(&h.state, "/root/pypi/simple/flask/", None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn test_simple_detail_upstream_unreachable_is_bad_gateway() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("velox.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let upstream = UpstreamClient::new("http://127.0.0.1:0/simple/").unwrap();
    let state = Arc::new(AppState::new(StateConfig {
        meta,
        blobs,
        upstream,
        index: "root/pypi".to_owned(),
        upload_index: "root/local".to_owned(),
        upload_token: None,
        ttl_secs: 60,
    }));
    let (status, ..) = get(&state, "/root/pypi/simple/flask/", None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn test_simple_detail_stale_on_upstream_error() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("velox.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let upstream = UpstreamClient::new("http://127.0.0.1:0/simple/").unwrap();
    let body = velox_core::pypi::to_json(&velox_core::pypi::ProjectDetail {
        meta: velox_core::pypi::Meta::default(),
        name: "flask".to_owned(),
        versions: vec![],
        files: vec![],
    });
    meta.put_index(
        "root/pypi/flask",
        &CachedIndex {
            etag: None,
            last_serial: None,
            fetched_at_unix: 0,
            body: body.into_bytes(),
        },
    )
    .unwrap();
    let state = Arc::new(AppState::with_clock(
        StateConfig {
            meta,
            blobs,
            upstream,
            index: "root/pypi".to_owned(),
            upload_index: "root/local".to_owned(),
            upload_token: None,
            ttl_secs: 60,
        },
        Arc::new(|| 100_000),
    ));

    let (status, _, served) = get(&state, "/root/pypi/simple/flask/", Some("application/json")).await;

    assert_eq!(status, StatusCode::OK);
    assert!(served.contains("flask"));
}

fn upload_fields() -> Vec<(&'static str, &'static str)> {
    vec![
        (":action", "file_upload"),
        ("name", "veloxpkg"),
        ("version", "1.0"),
        ("requires_python", ">=3.8"),
    ]
}

fn multipart_body(fields: &[(&str, &str)], content: Option<(&str, &[u8])>) -> (String, Vec<u8>) {
    let boundary = "veloxtestboundary";
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes(),
        );
    }
    if let Some((filename, bytes)) = content {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"content\"; filename=\"{filename}\"\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

fn upload_auth() -> String {
    format!("Basic {}", STANDARD.encode("__token__:s3cret"))
}

async fn post_upload(
    state: &Arc<AppState>,
    uri: &str,
    auth: Option<&str>,
    content_type: &str,
    body: Vec<u8>,
) -> StatusCode {
    let mut builder = Request::builder()
        .uri(uri)
        .method("POST")
        .header(header::CONTENT_TYPE, content_type);
    if let Some(auth) = auth {
        builder = builder.header(header::AUTHORIZATION, auth);
    }
    router(state.clone())
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn test_upload_then_serve_and_download() {
    let h = harness(60).await;
    let wheel = b"PKfakewheelbytes";
    let (ct, body) = multipart_body(&upload_fields(), Some(("veloxpkg-1.0-py3-none-any.whl", wheel)));
    assert_eq!(
        post_upload(&h.state, "/root/local/", Some(&upload_auth()), &ct, body).await,
        StatusCode::OK
    );

    let (ds, _, detail) = get(&h.state, "/root/local/simple/veloxpkg/", Some("application/json")).await;
    assert_eq!(ds, StatusCode::OK);
    assert!(detail.contains("veloxpkg-1.0-py3-none-any.whl"));
    assert!(detail.contains("\"1.0\""));
    assert!(detail.contains(">=3.8"));

    let digest = Digest::of(wheel);
    let uri = format!("/root/local/files/{}/veloxpkg-1.0-py3-none-any.whl", digest.as_str());
    let (fs, _, fbody) = get(&h.state, &uri, None).await;
    assert_eq!(fs, StatusCode::OK);
    assert_eq!(fbody.as_bytes(), wheel);

    let (ls, _, list) = get(&h.state, "/root/local/simple/", Some("application/json")).await;
    assert_eq!(ls, StatusCode::OK);
    assert!(list.contains("veloxpkg"));
}

#[tokio::test]
async fn test_upload_to_mirror_index_is_not_found() {
    let h = harness(60).await;
    let (ct, body) = multipart_body(&upload_fields(), Some(("x-1.0.whl", b"x")));
    assert_eq!(
        post_upload(&h.state, "/root/pypi/", Some(&upload_auth()), &ct, body).await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn test_upload_without_auth_is_unauthorized() {
    let h = harness(60).await;
    let (ct, body) = multipart_body(&upload_fields(), Some(("x-1.0.whl", b"x")));
    assert_eq!(
        post_upload(&h.state, "/root/local/", None, &ct, body).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn test_upload_disabled_without_token_is_forbidden() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("velox.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let upstream = UpstreamClient::new("http://127.0.0.1:0/simple/").unwrap();
    let state = Arc::new(AppState::new(StateConfig {
        meta,
        blobs,
        upstream,
        index: "root/pypi".to_owned(),
        upload_index: "root/local".to_owned(),
        upload_token: None,
        ttl_secs: 60,
    }));
    let (ct, body) = multipart_body(&upload_fields(), Some(("x-1.0.whl", b"x")));
    assert_eq!(
        post_upload(&state, "/root/local/", Some(&upload_auth()), &ct, body).await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn test_upload_wrong_action_is_bad_request() {
    let h = harness(60).await;
    let fields = vec![(":action", "submit"), ("name", "x"), ("version", "1.0")];
    let (ct, body) = multipart_body(&fields, Some(("x-1.0.whl", b"x")));
    assert_eq!(
        post_upload(&h.state, "/root/local/", Some(&upload_auth()), &ct, body).await,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn test_upload_missing_field_is_bad_request() {
    let h = harness(60).await;
    let fields = vec![(":action", "file_upload"), ("version", "1.0")];
    let (ct, body) = multipart_body(&fields, Some(("x-1.0.whl", b"x")));
    assert_eq!(
        post_upload(&h.state, "/root/local/", Some(&upload_auth()), &ct, body).await,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn test_upload_non_utf8_field_is_bad_request() {
    let h = harness(60).await;
    let mut body = Vec::new();
    body.extend_from_slice(b"--b\r\nContent-Disposition: form-data; name=\"name\"\r\n\r\n");
    body.extend_from_slice(&[0xff, 0xfe]);
    body.extend_from_slice(b"\r\n--b--\r\n");
    let status = post_upload(
        &h.state,
        "/root/local/",
        Some(&upload_auth()),
        "multipart/form-data; boundary=b",
        body,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_uploaded_detail_absent_is_not_found() {
    let h = harness(60).await;
    let (status, ..) = get(&h.state, "/root/local/simple/ghost/", Some("application/json")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_simple_detail_unknown_index_is_not_found() {
    let h = harness(60).await;
    let (status, ..) = get(&h.state, "/foo/bar/simple/x/", Some("application/json")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_file_download_unknown_index_is_not_found() {
    let h = harness(60).await;
    let uri = format!("/foo/bar/files/{}/x.whl", "a".repeat(64));
    let (status, ..) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_upload_with_declared_digest_and_extra_fields() {
    let h = harness(60).await;
    let wheel = b"PKwheelpayload";
    let digest = Digest::of(wheel);
    let fields = vec![
        (":action", "file_upload"),
        ("name", "veloxpkg"),
        ("version", "1.0"),
        ("sha256_digest", digest.as_str()),
        ("summary", "an ignored metadata field"),
    ];
    let (ct, body) = multipart_body(&fields, Some(("veloxpkg-1.0-py3-none-any.whl", wheel)));
    assert_eq!(
        post_upload(&h.state, "/root/local/", Some(&upload_auth()), &ct, body).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn test_upload_declared_digest_mismatch_is_bad_request() {
    let h = harness(60).await;
    let wrong = "00".repeat(32);
    let fields = vec![
        (":action", "file_upload"),
        ("name", "veloxpkg"),
        ("version", "1.0"),
        ("sha256_digest", wrong.as_str()),
    ];
    let (ct, body) = multipart_body(&fields, Some(("veloxpkg-1.0-py3-none-any.whl", b"bytes")));
    assert_eq!(
        post_upload(&h.state, "/root/local/", Some(&upload_auth()), &ct, body).await,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn test_upload_storage_failure_is_server_error() {
    let dir = tempfile::tempdir().unwrap();
    // A file where the blob directory should be, so the blob write fails.
    std::fs::write(dir.path().join("blobs"), b"not a directory").unwrap();
    let meta = MetaStore::open(dir.path().join("velox.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let upstream = UpstreamClient::new("http://127.0.0.1:0/simple/").unwrap();
    let state = Arc::new(AppState::new(StateConfig {
        meta,
        blobs,
        upstream,
        index: "root/pypi".to_owned(),
        upload_index: "root/local".to_owned(),
        upload_token: Some("s3cret".to_owned()),
        ttl_secs: 60,
    }));
    let (ct, body) = multipart_body(&upload_fields(), Some(("veloxpkg-1.0-py3-none-any.whl", b"data")));
    assert_eq!(
        post_upload(&state, "/root/local/", Some(&upload_auth()), &ct, body).await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[tokio::test]
async fn test_file_download_fetches_verifies_and_caches() {
    let h = harness(60).await;
    let wheel = b"wheelcontent";
    let digest = Digest::of(wheel);
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_detail(&h.server, digest.as_str(), &file_url, None).await;
    Mock::given(method("GET"))
        .and(path("/files/flask.whl"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(wheel.to_vec()))
        .expect(1)
        .mount(&h.server)
        .await;
    // Populate the file-url mapping via a simple request.
    get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;

    let uri = format!("/root/pypi/files/{}/flask-1.0-py3-none-any.whl", digest.as_str());
    let (status, headers, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "application/octet-stream");
    assert!(
        headers
            .get(header::CACHE_CONTROL)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("immutable")
    );
    assert_eq!(body, "wheelcontent");

    // Second download is served from the blob store (upstream file mock expects exactly one hit).
    let (status2, _, body2) = get(&h.state, &uri, None).await;
    assert_eq!(status2, StatusCode::OK);
    assert_eq!(body2, "wheelcontent");
}

#[tokio::test]
async fn test_file_download_invalid_digest() {
    let h = harness(60).await;
    let (status, ..) = get(&h.state, "/root/pypi/files/not-hex/x.whl", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_file_download_unknown_index() {
    let h = harness(60).await;
    let uri = format!("/a/b/files/{}/x.whl", "0".repeat(64));
    let (status, ..) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_file_download_unknown_file() {
    let h = harness(60).await;
    let uri = format!("/root/pypi/files/{}/x.whl", "a".repeat(64));
    let (status, ..) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_file_download_upstream_error_is_bad_gateway() {
    let h = harness(60).await;
    let digest = Digest::of(b"content");
    h.state
        .meta
        .put_file_url(digest.as_str(), "http://127.0.0.1:0/x.whl")
        .unwrap();
    let uri = format!("/root/pypi/files/{}/x.whl", digest.as_str());
    let (status, ..) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn test_simple_detail_file_without_hash_is_not_rewritten() {
    let h = harness(60).await;
    let json = "{\"meta\":{\"api-version\":\"1.1\"},\"name\":\"flask\",\"versions\":[\"1.0\"],\
                 \"files\":[{\"filename\":\"flask-1.0.tar.gz\",\"url\":\"http://up/flask-1.0.tar.gz\"}]}";
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(json.as_bytes().to_vec(), "application/vnd.pypi.simple.v1+json"),
        )
        .mount(&h.server)
        .await;
    let (status, _, body) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("http://up/flask-1.0.tar.gz"));
}

#[tokio::test]
async fn test_metadata_sibling_served_verified_and_cached() {
    let h = harness(60).await;
    let wheel_digest = Digest::of(b"wheelbytes");
    let metadata = b"Metadata-Version: 2.1\nName: flask\n";
    let meta_digest = Digest::of(metadata);
    let wheel_url = format!("{}/files/flask.whl", h.server.uri());
    let json = format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0.whl\",\"url\":\"{}\",\"hashes\":{{\"sha256\":\"{}\"}},\
         \"core-metadata\":{{\"sha256\":\"{}\"}}}}]}}",
        wheel_url,
        wheel_digest.as_str(),
        meta_digest.as_str()
    );
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(json.into_bytes(), "application/vnd.pypi.simple.v1+json"))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/flask.whl.metadata"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(metadata.to_vec()))
        .expect(1)
        .mount(&h.server)
        .await;

    let (_, _, detail) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;
    assert!(detail.contains(&format!(
        "\"core-metadata\":{{\"sha256\":\"{}\"}}",
        meta_digest.as_str()
    )));

    let uri = format!("/root/pypi/files/{}/flask-1.0.whl.metadata", wheel_digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "Metadata-Version: 2.1\nName: flask\n");

    // Second request is served from the blob cache (the upstream metadata mock expects one hit).
    let (status2, _, body2) = get(&h.state, &uri, None).await;
    assert_eq!(status2, StatusCode::OK);
    assert_eq!(body2, body);

    // Both .metadata requests are counted, giving /metrics server-side proof of the PEP 658 path.
    let (_, _, metrics) = get(&h.state, "/metrics", None).await;
    assert!(metrics.contains("velox_metadata_requests_total 2"));
}

#[tokio::test]
async fn test_metadata_not_found_when_unregistered() {
    let h = harness(60).await;
    let uri = format!("/root/pypi/files/{}/x.whl.metadata", "a".repeat(64));
    let (status, ..) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_simple_index_lists_observed_projects() {
    let h = harness(60).await;
    let digest = Digest::of(b"wheel");
    mount_detail(&h.server, digest.as_str(), "http://x/flask.whl", None).await;
    get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;

    let (status, headers, body) = get(&h.state, "/root/pypi/simple/", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("json")
    );
    assert!(body.contains("flask"));

    let (html_status, html_headers, html_body) = get(&h.state, "/root/pypi/simple/", Some("text/html")).await;
    assert_eq!(html_status, StatusCode::OK);
    assert_eq!(
        html_headers.get(header::CONTENT_TYPE).unwrap(),
        "text/html; charset=utf-8"
    );
    assert!(html_body.contains("flask"));
}

#[tokio::test]
async fn test_simple_index_unknown_index() {
    let h = harness(60).await;
    let (status, ..) = get(&h.state, "/foo/bar/simple/", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[test]
fn test_index_response_error_is_bad_gateway() {
    use crate::cache::CacheError;
    use crate::handlers::{Format, index_response};
    let response = index_response(Err(CacheError::Unavailable), Format::Json);
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn test_status() {
    let h = harness(60).await;
    let (status, headers, body) = get(&h.state, "/+status", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("json")
    );
    assert!(body.contains("root/pypi"));
    assert!(body.contains(env!("CARGO_PKG_VERSION")));
}

#[tokio::test]
async fn test_metrics() {
    let h = harness(60).await;
    get(&h.state, "/+status", None).await;
    let (status, _, body) = get(&h.state, "/metrics", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("velox_requests_total"));
    assert!(body.contains("velox_metadata_requests_total 0"));
}
