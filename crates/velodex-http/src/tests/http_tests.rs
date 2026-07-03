use std::io::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use http_body_util::BodyExt as _;
use tower::ServiceExt as _;
use velodex_storage::blob::{BlobStore, Digest};
use velodex_storage::meta::{CachedIndex, MetaStore};
use velodex_upstream::UpstreamClient;
use wiremock::matchers::{header as match_header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::router;
use crate::state::{AppState, Index, IndexKind};

pub(super) struct Harness {
    _dir: tempfile::TempDir,
    pub(super) server: MockServer,
    pub(super) state: Arc<AppState>,
    pub(super) clock: Arc<AtomicI64>,
}

/// A mirror (`pypi`) proxying the mock, a local store (`local`), and an overlay (`root/pypi`) that
/// layers the local store in front of the mirror. `token`/`volatile` tune the local store.
async fn harness_with(token: bool, volatile: bool) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let meta = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let upstream = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let clock = Arc::new(AtomicI64::new(1000));
    let ticks = clock.clone();
    let indexes = vec![
        Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            kind: IndexKind::Mirror(upstream),
        },
        Index {
            name: "local".to_owned(),
            route: "local".to_owned(),
            kind: IndexKind::Local {
                upload_token: token.then(|| "s3cret".to_owned()),
                volatile,
            },
        },
        Index {
            name: "root/pypi".to_owned(),
            route: "root/pypi".to_owned(),
            kind: IndexKind::Overlay {
                layers: vec![1, 0],
                upload: Some(1),
            },
        },
    ];
    let state = Arc::new(AppState::with_clock(
        meta,
        blobs,
        60,
        indexes,
        Arc::new(move || ticks.load(Ordering::Relaxed)),
    ));
    Harness {
        _dir: dir,
        server,
        state,
        clock,
    }
}

pub(super) async fn harness() -> Harness {
    harness_with(true, true).await
}

pub(super) async fn get(state: &Arc<AppState>, uri: &str, accept: Option<&str>) -> (StatusCode, HeaderMap, String) {
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

async fn request(state: &Arc<AppState>, verb: &str, uri: &str, auth: Option<&str>) -> StatusCode {
    let mut builder = Request::builder().uri(uri).method(verb);
    if let Some(auth) = auth {
        builder = builder.header(header::AUTHORIZATION, auth);
    }
    router(state.clone())
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

pub(super) fn detail_json(digest: &str, file_url: &str) -> String {
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
async fn test_mirror_detail_json_rewrites_file_url_and_caches() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_detail(&h.server, digest.as_str(), &file_url, None).await;

    let (status, headers, body) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "application/vnd.pypi.simple.v1+json"
    );
    assert_eq!(headers.get(header::VARY).unwrap(), "Accept");
    assert!(body.contains(&format!("/pypi/files/{}/flask-1.0-py3-none-any.whl", digest.as_str())));

    // Second request within the TTL is a cache hit.
    let (status2, ..) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    assert_eq!(status2, StatusCode::OK);
}

#[tokio::test]
async fn test_mirror_detail_html() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    mount_detail(&h.server, digest.as_str(), "http://x/flask.whl", None).await;

    let (status, headers, body) = get(&h.state, "/pypi/simple/flask/", Some("text/html")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "text/html; charset=utf-8");
    assert!(body.contains("<a href="));
}

#[tokio::test]
async fn test_mirror_detail_from_html_only_upstream() {
    let h = harness().await;
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

    let (status, _, body) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(&format!("/pypi/files/{}/flask-1.0.whl", digest.as_str())));
}

#[tokio::test]
async fn test_unknown_route_is_not_found() {
    let h = harness().await;
    let (status, ..) = get(&h.state, "/nope/simple/flask/", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_mirror_detail_upstream_404() {
    let h = harness().await;
    Mock::given(method("GET"))
        .and(path("/simple/missing/"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&h.server)
        .await;
    let (status, ..) = get(&h.state, "/pypi/simple/missing/", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_mirror_detail_revalidate_304_serves_cached() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    mount_detail(&h.server, digest.as_str(), "http://x/flask.whl", Some("\"v1\"")).await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .and(match_header("if-none-match", "\"v1\""))
        .respond_with(ResponseTemplate::new(304))
        .with_priority(1)
        .mount(&h.server)
        .await;

    let (first, ..) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    assert_eq!(first, StatusCode::OK);

    h.clock.store(5000, Ordering::Relaxed); // stale, forces revalidation
    let (second, _, body) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    assert_eq!(second, StatusCode::OK);
    assert!(body.contains("flask"));
}

#[tokio::test]
async fn test_mirror_detail_stale_on_5xx() {
    let h = harness().await;
    let body = velodex_core::pypi::to_json(&velodex_core::pypi::ProjectDetail {
        meta: velodex_core::pypi::Meta::default(),
        name: "flask".to_owned(),
        versions: vec!["1.0".to_owned()],
        files: vec![],
    });
    h.state
        .meta
        .put_index(
            "pypi/flask",
            &CachedIndex {
                etag: None,
                last_serial: None,
                fetched_at_unix: 0,
                content_type: None,

                fresh_secs: None,
                body: body.into_bytes(),
            },
        )
        .unwrap();
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&h.server)
        .await;
    let (status, ..) = get(&h.state, "/pypi/simple/flask/", None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn test_mirror_detail_upstream_unreachable_is_bad_gateway() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let upstream = UpstreamClient::new("http://127.0.0.1:0/simple/").unwrap();
    let indexes = vec![Index {
        name: "pypi".to_owned(),
        route: "pypi".to_owned(),
        kind: IndexKind::Mirror(upstream),
    }];
    let state = Arc::new(AppState::new(meta, blobs, 60, indexes));
    let (status, ..) = get(&state, "/pypi/simple/flask/", None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn test_mirror_detail_stale_on_upstream_error() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let upstream = UpstreamClient::new("http://127.0.0.1:0/simple/").unwrap();
    let body = velodex_core::pypi::to_json(&velodex_core::pypi::ProjectDetail {
        meta: velodex_core::pypi::Meta::default(),
        name: "flask".to_owned(),
        versions: vec![],
        files: vec![],
    });
    meta.put_index(
        "pypi/flask",
        &CachedIndex {
            etag: None,
            last_serial: None,
            fetched_at_unix: 0,
            content_type: None,

            fresh_secs: None,
            body: body.into_bytes(),
        },
    )
    .unwrap();
    let indexes = vec![Index {
        name: "pypi".to_owned(),
        route: "pypi".to_owned(),
        kind: IndexKind::Mirror(upstream),
    }];
    let state = Arc::new(AppState::with_clock(meta, blobs, 60, indexes, Arc::new(|| 100_000)));
    let (status, _, served) = get(&state, "/pypi/simple/flask/", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(served.contains("flask"));
}

#[tokio::test]
async fn test_file_download_fetches_verifies_and_caches() {
    let h = harness().await;
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

    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await; // registers the file url
    let uri = format!("/pypi/files/{}/flask-1.0-py3-none-any.whl", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "wheelcontent");
    let (status2, _, body2) = get(&h.state, &uri, None).await; // second from blob cache
    assert_eq!(status2, StatusCode::OK);
    assert_eq!(body2, body);
}

#[tokio::test]
async fn test_file_download_invalid_digest_is_bad_request() {
    let h = harness().await;
    let (status, ..) = get(&h.state, "/pypi/files/notahex/x.whl", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_file_download_unknown_digest_is_not_found() {
    let h = harness().await;
    let uri = format!("/pypi/files/{}/x.whl", "a".repeat(64));
    let (status, ..) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_metadata_served_verified_and_counted() {
    let h = harness().await;
    let wheel_digest = Digest::of(b"wheel-bytes");
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

    let (_, _, detail) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    assert!(detail.contains(&format!(
        "\"core-metadata\":{{\"sha256\":\"{}\"}}",
        meta_digest.as_str()
    )));

    let uri = format!("/pypi/files/{}/flask-1.0.whl.metadata", wheel_digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "Metadata-Version: 2.1\nName: flask\n");
    let (status2, _, body2) = get(&h.state, &uri, None).await; // cached
    assert_eq!(status2, StatusCode::OK);
    assert_eq!(body2, body);

    let (_, _, metrics) = get(&h.state, "/metrics", None).await;
    assert!(metrics.contains("velodex_metadata_requests_total 2"));
}

#[tokio::test]
async fn test_metadata_not_found_when_unregistered() {
    let h = harness().await;
    let uri = format!("/pypi/files/{}/x.whl.metadata", "a".repeat(64));
    let (status, ..) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

fn upload_fields() -> Vec<(&'static str, &'static str)> {
    vec![
        (":action", "file_upload"),
        ("name", "velodexpkg"),
        ("version", "1.0"),
        ("requires_python", ">=3.8"),
    ]
}

fn multipart_body(fields: &[(&str, &str)], content: Option<(&str, &[u8])>) -> (String, Vec<u8>) {
    let boundary = "velodextestboundary";
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

async fn upload_velodexpkg(state: &Arc<AppState>, uri: &str, wheel: &[u8]) -> StatusCode {
    let (ct, body) = multipart_body(&upload_fields(), Some(("velodexpkg-1.0-py3-none-any.whl", wheel)));
    post_upload(state, uri, Some(&upload_auth()), &ct, body).await
}

#[tokio::test]
async fn test_upload_via_overlay_then_serve_and_download() {
    let h = harness().await;
    let wheel = b"PKuploadedwheel";
    assert_eq!(upload_velodexpkg(&h.state, "/root/pypi/", wheel).await, StatusCode::OK);

    // Served through the overlay, with the URL on the overlay route.
    let (ds, _, detail) = get(&h.state, "/root/pypi/simple/velodexpkg/", Some("application/json")).await;
    assert_eq!(ds, StatusCode::OK);
    assert!(detail.contains("velodexpkg-1.0-py3-none-any.whl"));
    assert!(detail.contains("\"1.0\""));
    let digest = Digest::of(wheel);
    assert!(detail.contains(&format!("/root/pypi/files/{}/velodexpkg", digest.as_str())));

    let uri = format!("/root/pypi/files/{}/velodexpkg-1.0-py3-none-any.whl", digest.as_str());
    let (fs, _, fbody) = get(&h.state, &uri, None).await;
    assert_eq!(fs, StatusCode::OK);
    assert_eq!(fbody.as_bytes(), wheel);

    // The overlay's project list includes the uploaded project.
    let (ls, _, list) = get(&h.state, "/root/pypi/simple/", Some("application/json")).await;
    assert_eq!(ls, StatusCode::OK);
    assert!(list.contains("velodexpkg"));
}

#[tokio::test]
async fn test_overlay_tolerates_unavailable_layer() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let upstream = UpstreamClient::new("http://127.0.0.1:0/simple/").unwrap();
    let indexes = vec![
        Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            kind: IndexKind::Mirror(upstream),
        },
        Index {
            name: "local".to_owned(),
            route: "local".to_owned(),
            kind: IndexKind::Local {
                upload_token: Some("s3cret".to_owned()),
                volatile: true,
            },
        },
        Index {
            name: "root/pypi".to_owned(),
            route: "root/pypi".to_owned(),
            kind: IndexKind::Overlay {
                layers: vec![1, 0],
                upload: Some(1),
            },
        },
    ];
    let state = Arc::new(AppState::new(meta, blobs, 60, indexes));
    upload_velodexpkg(&state, "/root/pypi/", b"x").await;
    // The mirror layer is unreachable, but the local layer still serves the upload.
    let (status, _, detail) = get(&state, "/root/pypi/simple/velodexpkg/", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(detail.contains("velodexpkg"));
}

#[tokio::test]
async fn test_upload_direct_to_local_route() {
    let h = harness().await;
    assert_eq!(upload_velodexpkg(&h.state, "/local/", b"bytes").await, StatusCode::OK);
    let (status, _, detail) = get(&h.state, "/local/simple/velodexpkg/", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(detail.contains("velodexpkg"));
}

#[tokio::test]
async fn test_upload_to_mirror_route_is_method_not_allowed() {
    let h = harness().await;
    assert_eq!(
        upload_velodexpkg(&h.state, "/pypi/", b"x").await,
        StatusCode::METHOD_NOT_ALLOWED
    );
}

#[tokio::test]
async fn test_upload_unknown_route_is_not_found() {
    let h = harness().await;
    assert_eq!(upload_velodexpkg(&h.state, "/nope/", b"x").await, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_upload_to_subpath_is_not_found() {
    let h = harness().await;
    assert_eq!(
        upload_velodexpkg(&h.state, "/local/simple/", b"x").await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn test_upload_without_auth_is_unauthorized() {
    let h = harness().await;
    let (ct, body) = multipart_body(&upload_fields(), Some(("x-1.0.whl", b"x")));
    assert_eq!(
        post_upload(&h.state, "/root/pypi/", None, &ct, body).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn test_upload_disabled_without_token_is_forbidden() {
    let h = harness_with(false, true).await;
    assert_eq!(
        upload_velodexpkg(&h.state, "/root/pypi/", b"x").await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn test_upload_wrong_action_is_bad_request() {
    let h = harness().await;
    let fields = vec![(":action", "submit"), ("name", "x"), ("version", "1.0")];
    let (ct, body) = multipart_body(&fields, Some(("x-1.0.whl", b"x")));
    assert_eq!(
        post_upload(&h.state, "/root/pypi/", Some(&upload_auth()), &ct, body).await,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn test_upload_missing_field_is_bad_request() {
    let h = harness().await;
    let fields = vec![(":action", "file_upload"), ("version", "1.0")];
    let (ct, body) = multipart_body(&fields, Some(("x-1.0.whl", b"x")));
    assert_eq!(
        post_upload(&h.state, "/root/pypi/", Some(&upload_auth()), &ct, body).await,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn test_upload_non_utf8_field_is_bad_request() {
    let h = harness().await;
    let mut body = Vec::new();
    body.extend_from_slice(b"--b\r\nContent-Disposition: form-data; name=\"name\"\r\n\r\n");
    body.extend_from_slice(&[0xff, 0xfe]);
    body.extend_from_slice(b"\r\n--b--\r\n");
    let status = post_upload(
        &h.state,
        "/root/pypi/",
        Some(&upload_auth()),
        "multipart/form-data; boundary=b",
        body,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_upload_declared_digest_mismatch_is_bad_request() {
    let h = harness().await;
    let wrong = "00".repeat(32);
    let fields = vec![
        (":action", "file_upload"),
        ("name", "velodexpkg"),
        ("version", "1.0"),
        ("sha256_digest", wrong.as_str()),
    ];
    let (ct, body) = multipart_body(&fields, Some(("velodexpkg-1.0.whl", b"bytes")));
    assert_eq!(
        post_upload(&h.state, "/root/pypi/", Some(&upload_auth()), &ct, body).await,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn test_upload_with_declared_digest_and_extra_field() {
    let h = harness().await;
    let wheel = b"PKwheelpayload";
    let digest = Digest::of(wheel);
    let fields = vec![
        (":action", "file_upload"),
        ("name", "velodexpkg"),
        ("version", "1.0"),
        ("sha256_digest", digest.as_str()),
        ("summary", "ignored"),
    ];
    let (ct, body) = multipart_body(&fields, Some(("velodexpkg-1.0.whl", wheel)));
    assert_eq!(
        post_upload(&h.state, "/root/pypi/", Some(&upload_auth()), &ct, body).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn test_upload_storage_failure_is_server_error() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("blobs"), b"not a directory").unwrap();
    let meta = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let indexes = vec![Index {
        name: "local".to_owned(),
        route: "local".to_owned(),
        kind: IndexKind::Local {
            upload_token: Some("s3cret".to_owned()),
            volatile: true,
        },
    }];
    let state = Arc::new(AppState::new(meta, blobs, 60, indexes));
    assert_eq!(
        upload_velodexpkg(&state, "/local/", b"data").await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[tokio::test]
async fn test_yank_and_unyank_and_delete() {
    let h = harness().await;
    upload_velodexpkg(&h.state, "/root/pypi/", b"PKyankme").await;

    // Yank the version, then the file is served with a yank marker.
    assert_eq!(
        request(&h.state, "PUT", "/root/pypi/velodexpkg/1.0/yank", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (_, _, yanked) = get(&h.state, "/root/pypi/simple/velodexpkg/", Some("application/json")).await;
    assert!(yanked.contains("\"yanked\":true"));

    // Un-yank via DELETE .../yank.
    assert_eq!(
        request(
            &h.state,
            "DELETE",
            "/root/pypi/velodexpkg/1.0/yank",
            Some(&upload_auth())
        )
        .await,
        StatusCode::OK
    );
    let (_, _, unyanked) = get(&h.state, "/root/pypi/simple/velodexpkg/", Some("application/json")).await;
    assert!(!unyanked.contains("\"yanked\":true"));

    // Delete the whole project.
    assert_eq!(
        request(&h.state, "DELETE", "/root/pypi/velodexpkg/", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (status, ..) = get(&h.state, "/root/pypi/simple/velodexpkg/", Some("application/json")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_delete_specific_version() {
    let h = harness().await;
    upload_velodexpkg(&h.state, "/local/", b"PKv1").await;
    assert_eq!(
        request(&h.state, "DELETE", "/local/velodexpkg/1.0/", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (status, ..) = get(&h.state, "/local/simple/velodexpkg/", Some("application/json")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_delete_nonexistent_is_not_found() {
    let h = harness().await;
    assert_eq!(
        request(&h.state, "DELETE", "/local/ghost/", Some(&upload_auth())).await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn test_delete_requires_auth() {
    let h = harness().await;
    upload_velodexpkg(&h.state, "/local/", b"x").await;
    assert_eq!(
        request(&h.state, "DELETE", "/local/velodexpkg/", None).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn test_delete_on_non_volatile_is_forbidden() {
    let h = harness_with(true, false).await;
    upload_velodexpkg(&h.state, "/local/", b"x").await;
    assert_eq!(
        request(&h.state, "DELETE", "/local/velodexpkg/", Some(&upload_auth())).await,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn test_delete_on_mirror_route_is_method_not_allowed() {
    let h = harness().await;
    assert_eq!(
        request(&h.state, "DELETE", "/pypi/flask/", Some(&upload_auth())).await,
        StatusCode::METHOD_NOT_ALLOWED
    );
}

#[tokio::test]
async fn test_put_without_yank_suffix_is_not_found() {
    let h = harness().await;
    assert_eq!(
        request(&h.state, "PUT", "/local/velodexpkg/1.0/", Some(&upload_auth())).await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn test_yank_on_mirror_route_is_method_not_allowed() {
    let h = harness().await;
    let status = request(&h.state, "PUT", "/pypi/flask/1.0/yank", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn test_longest_prefix_wins() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    // Routes "a" and "a/b" both prefix "a/b/simple/"; the longer must win.
    let indexes = vec![
        Index {
            name: "a".to_owned(),
            route: "a".to_owned(),
            kind: IndexKind::Local {
                upload_token: None,
                volatile: true,
            },
        },
        Index {
            name: "ab".to_owned(),
            route: "a/b".to_owned(),
            kind: IndexKind::Local {
                upload_token: Some("s3cret".to_owned()),
                volatile: true,
            },
        },
    ];
    let state = Arc::new(AppState::new(meta, blobs, 60, indexes));
    // Uploading requires a token; only "a/b" has one, so a 401-vs-200 proves which matched.
    assert_eq!(upload_velodexpkg(&state, "/a/b/", b"x").await, StatusCode::OK);
}

#[tokio::test]
async fn test_status_lists_routes() {
    let h = harness().await;
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
async fn test_metrics_exposes_counters() {
    let h = harness().await;
    get(&h.state, "/+status", None).await;
    let (status, _, body) = get(&h.state, "/metrics", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("velodex_requests_total"));
    assert!(body.contains("velodex_metadata_requests_total 0"));
}

#[tokio::test]
async fn test_metrics_exposes_per_index_counters() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_detail(&h.server, digest.as_str(), &file_url, None).await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    for _ in 0..500 {
        if h.state.metrics.index_totals().contains_key("pypi") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    // A second route makes the exposition ordering observable.
    h.state.metrics.record(crate::metrics::Event::Page {
        route: "local".to_owned(),
        project: "veloxpkg".to_owned(),
    });
    for _ in 0..500 {
        if h.state.metrics.index_totals().len() == 2 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let (status, _, body) = get(&h.state, "/metrics", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("velodex_index_pages_total{index=\"local\"} 1"));
    assert!(body.contains("velodex_index_pages_total{index=\"pypi\"} 1"));
    assert!(body.contains("velodex_index_refreshes_total{index=\"pypi\"} 0"));
    assert!(body.contains("velodex_index_rejected_total{index=\"pypi\"} 0"));
}

#[test]
fn test_index_response_error_is_bad_gateway() {
    use crate::cache::CacheError;
    use crate::handlers::{Format, index_response};
    let response = index_response(Err(CacheError::Unavailable), Format::Json);
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
}

async fn upload_version(state: &Arc<AppState>, uri: &str, version: &str, wheel: &[u8]) -> StatusCode {
    let fields = vec![(":action", "file_upload"), ("name", "velodexpkg"), ("version", version)];
    let filename = format!("velodexpkg-{version}-py3-none-any.whl");
    let (ct, body) = multipart_body(&fields, Some((&filename, wheel)));
    post_upload(state, uri, Some(&upload_auth()), &ct, body).await
}

#[tokio::test]
async fn test_mirror_5xx_without_cache_is_bad_gateway() {
    let h = harness().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&h.server)
        .await;
    let (status, ..) = get(&h.state, "/pypi/simple/flask/", None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn test_mirror_file_without_sha_is_kept() {
    let h = harness().await;
    let json = "{\"meta\":{\"api-version\":\"1.1\"},\"name\":\"flask\",\"versions\":[\"1.0\"],\
                \"files\":[{\"filename\":\"flask-1.0.tar.gz\",\"url\":\"http://x/flask-1.0.tar.gz\",\"hashes\":{}}]}";
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(json.as_bytes().to_vec(), "application/vnd.pypi.simple.v1+json"),
        )
        .mount(&h.server)
        .await;
    let (status, _, body) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("flask-1.0.tar.gz"));
}

#[tokio::test]
async fn test_file_source_not_a_mirror_is_not_found() {
    let h = harness().await;
    let digest = Digest::of(b"orphan");
    h.state
        .meta
        .put_file_url(digest.as_str(), "http://x/orphan.whl", "local")
        .unwrap();
    let uri = format!("/pypi/files/{}/orphan.whl", digest.as_str());
    let (status, ..) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_file_digest_mismatch_fails_the_body_and_never_persists() {
    let h = harness().await;
    let digest = Digest::of(b"expected");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_detail(&h.server, digest.as_str(), &file_url, None).await;
    Mock::given(method("GET"))
        .and(path("/files/flask.whl"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"wrong bytes".to_vec()))
        .mount(&h.server)
        .await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    let uri = format!("/pypi/files/{}/flask-1.0-py3-none-any.whl", digest.as_str());
    // The transfer fails verification, so the body errors instead of completing…
    let response = router(h.state.clone())
        .oneshot(Request::builder().uri(&*uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.into_body().collect().await.is_err());
    // …the corrupt blob is never admitted into the store, and the rejection is counted. The poll
    // must yield to the runtime: the detached transfer task records the rejection, and a blocking
    // sleep would starve it on the single-threaded test runtime.
    for _ in 0..500 {
        let totals = h.state.metrics.index_totals();
        if totals.get("pypi").is_some_and(|t| t.rejected == 1) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    assert!(!h.state.blobs.exists(&digest));
    assert_eq!(h.state.metrics.index_totals()["pypi"].rejected, 1);
}

#[tokio::test]
async fn test_delete_one_of_two_versions() {
    let h = harness().await;
    upload_version(&h.state, "/local/", "1.0", b"PKv1").await;
    upload_version(&h.state, "/local/", "2.0", b"PKv2").await;
    assert_eq!(
        request(&h.state, "DELETE", "/local/velodexpkg/1.0/", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (_, _, detail) = get(&h.state, "/local/simple/velodexpkg/", Some("application/json")).await;
    assert!(detail.contains("2.0"));
    assert!(!detail.contains("velodexpkg-1.0"));
}

#[tokio::test]
async fn test_yank_one_of_two_versions() {
    let h = harness().await;
    upload_version(&h.state, "/local/", "1.0", b"PKv1").await;
    upload_version(&h.state, "/local/", "2.0", b"PKv2").await;
    assert_eq!(
        request(&h.state, "PUT", "/local/velodexpkg/1.0/yank", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (_, _, detail) = get(&h.state, "/local/simple/velodexpkg/", Some("application/json")).await;
    // Only the 1.0 file carries the yank marker.
    assert_eq!(detail.matches("\"yanked\":true").count(), 1);
}

#[tokio::test]
async fn test_file_path_without_filename_is_not_found() {
    let h = harness().await;
    let (status, ..) = get(&h.state, "/pypi/files/onlyonesegment", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_get_unrecognized_subpath_is_not_found() {
    let h = harness().await;
    let (status, ..) = get(&h.state, "/pypi/random/", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_get_route_without_trailing_slash_is_not_found() {
    let h = harness().await;
    let (status, ..) = get(&h.state, "/pypi", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_project_list_html() {
    let h = harness().await;
    upload_velodexpkg(&h.state, "/local/", b"x").await;
    let (status, headers, body) = get(&h.state, "/local/simple/", Some("text/html")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "text/html; charset=utf-8");
    assert!(body.contains("velodexpkg"));
}

#[tokio::test]
async fn test_removal_storage_error_is_server_error() {
    let h = harness().await;
    h.state
        .meta
        .put_upload("local", "velodexpkg", "velodexpkg-1.0.whl", b"{ not json")
        .unwrap();
    // A versioned delete must decode each record to filter, so the corrupt record errors.
    let status = request(&h.state, "DELETE", "/local/velodexpkg/1.0/", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn test_upload_target_resolving_to_non_local_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let upstream = UpstreamClient::new("http://127.0.0.1:0/simple/").unwrap();
    // A deliberately inconsistent overlay whose upload target points at the mirror.
    let indexes = vec![
        Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            kind: IndexKind::Mirror(upstream),
        },
        Index {
            name: "ov".to_owned(),
            route: "ov".to_owned(),
            kind: IndexKind::Overlay {
                layers: vec![0],
                upload: Some(0),
            },
        },
    ];
    let state = Arc::new(AppState::new(meta, blobs, 60, indexes));
    assert_eq!(upload_velodexpkg(&state, "/ov/", b"x").await, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_openapi_endpoint_serves_the_document() {
    let h = harness().await;
    let (status, headers, body) = get(&h.state, "/api-docs/openapi.json", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "application/json");
    let spec: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(spec["openapi"], "3.1.0");
}

#[tokio::test]
async fn test_yank_upstream_file_via_overlay() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    mount_detail(&h.server, digest.as_str(), "http://x/flask-1.0-py3-none-any.whl", None).await;

    let status = request(&h.state, "PUT", "/root/pypi/flask/1.0/yank", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::OK);

    // The overlay page carries the marker; the mirror's own route stays untouched.
    let (_, _, merged) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;
    assert!(merged.contains("\"yanked\":true"));
    let (_, _, mirror) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    assert!(!mirror.contains("\"yanked\":true"));

    // Un-yank clears the override.
    let status = request(&h.state, "DELETE", "/root/pypi/flask/1.0/yank", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, cleared) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;
    assert!(!cleared.contains("\"yanked\":true"));
}

#[tokio::test]
async fn test_delete_and_restore_upstream_file_via_overlay() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    mount_detail(&h.server, digest.as_str(), "http://x/flask-1.0-py3-none-any.whl", None).await;

    let status = request(&h.state, "DELETE", "/root/pypi/flask/", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::OK);

    // Hidden from the overlay page, but still present on the mirror's own route.
    let (_, _, merged) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;
    assert!(!merged.contains("flask-1.0-py3-none-any.whl"));
    let (_, _, mirror) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    assert!(mirror.contains("flask-1.0-py3-none-any.whl"));

    // Restore brings the file back.
    let status = request(&h.state, "PUT", "/root/pypi/flask/restore", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, restored) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;
    assert!(restored.contains("flask-1.0-py3-none-any.whl"));
}

#[tokio::test]
async fn test_delete_one_upstream_version_leaves_other() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let json = format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"flask\",\"versions\":[\"1.0\",\"2.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\"url\":\"http://x/a.whl\",\
         \"hashes\":{{\"sha256\":\"{digest}\"}}}},\
         {{\"filename\":\"flask-2.0-py3-none-any.whl\",\"url\":\"http://x/b.whl\",\
         \"hashes\":{{\"sha256\":\"{digest}\"}}}}]}}",
        digest = digest.as_str()
    );
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(json.into_bytes(), "application/vnd.pypi.simple.v1+json"))
        .mount(&h.server)
        .await;

    let status = request(&h.state, "DELETE", "/root/pypi/flask/1.0/", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, merged) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;
    assert!(!merged.contains("flask-1.0-py3-none-any.whl"));
    assert!(merged.contains("flask-2.0-py3-none-any.whl"));
}

#[tokio::test]
async fn test_restore_with_nothing_hidden_is_not_found() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    mount_detail(&h.server, digest.as_str(), "http://x/flask-1.0-py3-none-any.whl", None).await;
    let status = request(&h.state, "PUT", "/root/pypi/flask/restore", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_put_unknown_suffix_is_not_found() {
    let h = harness().await;
    let status = request(&h.state, "PUT", "/root/pypi/flask/1.0/promote", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_delete_upstream_on_non_volatile_still_hides() {
    let h = harness_with(true, false).await;
    let digest = Digest::of(b"wheel");
    mount_detail(&h.server, digest.as_str(), "http://x/flask-1.0-py3-none-any.whl", None).await;
    // Hiding an upstream file is reversible, so it works even when uploads are immutable.
    let status = request(&h.state, "DELETE", "/root/pypi/flask/", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::OK);
}

fn fixture_wheel() -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("velodexpkg/__init__.py", options).unwrap();
        zip.write_all(b"VALUE = 1\n").unwrap();
        zip.start_file("velodexpkg-1.0.dist-info/METADATA", options).unwrap();
        zip.write_all(b"Metadata-Version: 2.1\nName: velodexpkg\nVersion: 1.0\n")
            .unwrap();
        zip.finish().unwrap();
    }
    buf
}

async fn upload_wheel(state: &Arc<AppState>, filename: &str, bytes: &[u8]) -> Digest {
    let fields = vec![(":action", "file_upload"), ("name", "velodexpkg"), ("version", "1.0")];
    let (ct, body) = multipart_body(&fields, Some((filename, bytes)));
    assert_eq!(
        post_upload(state, "/local/", Some(&upload_auth()), &ct, body).await,
        StatusCode::OK
    );
    Digest::of(bytes)
}

#[tokio::test]
async fn test_inspect_lists_wheel_members() {
    let h = harness().await;
    let digest = upload_wheel(&h.state, "velodexpkg-1.0-py3-none-any.whl", &fixture_wheel()).await;
    let uri = format!("/local/inspect/{}/velodexpkg-1.0-py3-none-any.whl", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    let listing: serde_json::Value = serde_json::from_str(&body).unwrap();
    let paths: Vec<&str> = listing["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|member| member["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["velodexpkg-1.0.dist-info/METADATA", "velodexpkg/__init__.py"]);
}

#[tokio::test]
async fn test_inspect_reads_member_content() {
    let h = harness().await;
    let digest = upload_wheel(&h.state, "velodexpkg-1.0-py3-none-any.whl", &fixture_wheel()).await;
    let uri = format!(
        "/local/inspect/{}/velodexpkg-1.0-py3-none-any.whl/velodexpkg-1.0.dist-info/METADATA",
        digest.as_str()
    );
    let (status, headers, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "text/plain; charset=utf-8");
    assert!(body.starts_with("Metadata-Version: 2.1"));
}

#[tokio::test]
async fn test_inspect_missing_member_is_not_found() {
    let h = harness().await;
    let digest = upload_wheel(&h.state, "velodexpkg-1.0-py3-none-any.whl", &fixture_wheel()).await;
    let uri = format!(
        "/local/inspect/{}/velodexpkg-1.0-py3-none-any.whl/nope.py",
        digest.as_str()
    );
    let (status, ..) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_inspect_unsupported_type() {
    let h = harness().await;
    let digest = upload_wheel(&h.state, "velodexpkg-1.0.txt", b"not an archive").await;
    let uri = format!("/local/inspect/{}/velodexpkg-1.0.txt", digest.as_str());
    let (status, ..) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn test_inspect_corrupt_archive_is_unprocessable() {
    let h = harness().await;
    let digest = upload_wheel(&h.state, "velodexpkg-1.0-py3-none-any.whl", b"PK corrupt bytes").await;
    let uri = format!("/local/inspect/{}/velodexpkg-1.0-py3-none-any.whl", digest.as_str());
    let (status, ..) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn test_inspect_tarball_and_size_limit() {
    let h = harness().await;
    // A gzipped tarball with one small file and one over the inline limit.
    let mut tarball = Vec::new();
    {
        let encoder = flate2::write::GzEncoder::new(&mut tarball, flate2::Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        let small = b"print()\n";
        let mut head = tar::Header::new_gnu();
        head.set_size(small.len() as u64);
        head.set_cksum();
        builder
            .append_data(&mut head, "velodexpkg-1.0/setup.py", &small[..])
            .unwrap();
        let big = vec![0_u8; usize::try_from(crate::archive::MEMBER_LIMIT + 1).unwrap()];
        let mut head = tar::Header::new_gnu();
        head.set_size(big.len() as u64);
        head.set_cksum();
        builder
            .append_data(&mut head, "velodexpkg-1.0/big.bin", big.as_slice())
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap();
    }
    let digest = upload_wheel(&h.state, "velodexpkg-1.0.tar.gz", &tarball).await;

    let uri = format!("/local/inspect/{}/velodexpkg-1.0.tar.gz", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("setup.py"));

    let (status, _, content) = get(&h.state, &format!("{uri}/velodexpkg-1.0/setup.py"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content, "print()\n");

    let (status, ..) = get(&h.state, &format!("{uri}/velodexpkg-1.0/big.bin"), None).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn test_inspect_binary_member_served_as_bytes() {
    let h = harness().await;
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("data.bin", options).unwrap();
        zip.write_all(&[0xff, 0xfe, 0x00]).unwrap();
        zip.finish().unwrap();
    }
    let digest = upload_wheel(&h.state, "velodexpkg-1.0-py3-none-any.whl", &buf).await;
    let uri = format!(
        "/local/inspect/{}/velodexpkg-1.0-py3-none-any.whl/data.bin",
        digest.as_str()
    );
    let (status, headers, _) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "application/octet-stream");
}

#[tokio::test]
async fn test_inspect_bad_digest_and_missing_paths() {
    let h = harness().await;
    let (status, ..) = get(&h.state, "/local/inspect/nothex/x.whl", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, ..) = get(&h.state, "/local/inspect/onlyonesegment", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let uri = format!("/local/inspect/{}/ghost.whl", "a".repeat(64));
    let (status, ..) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_upload_wheel_gains_metadata_sibling() {
    let h = harness().await;
    let digest = upload_wheel(&h.state, "velodexpkg-1.0-py3-none-any.whl", &fixture_wheel()).await;
    // The simple page advertises the extracted PEP 658 sibling, and it is servable.
    let (_, _, detail) = get(&h.state, "/local/simple/velodexpkg/", Some("application/json")).await;
    assert!(detail.contains("\"core-metadata\":{\"sha256\""));
    let uri = format!(
        "/local/files/{}/velodexpkg-1.0-py3-none-any.whl.metadata",
        digest.as_str()
    );
    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.starts_with("Metadata-Version: 2.1"));
}

#[tokio::test]
async fn test_overlay_upload_only_project_unknown_elsewhere() {
    let h = harness().await;
    // Upstream 404s for the project: only the local layer answers, exercising the not-found layer path.
    Mock::given(method("GET"))
        .and(path("/simple/velodexpkg/"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&h.server)
        .await;
    upload_wheel(&h.state, "velodexpkg-1.0-py3-none-any.whl", &fixture_wheel()).await;
    let (status, _, detail) = get(&h.state, "/root/pypi/simple/velodexpkg/", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(detail.contains("velodexpkg-1.0-py3-none-any.whl"));
}

#[tokio::test]
async fn test_yank_overlay_with_uploaded_file_skips_override() {
    let h = harness().await;
    Mock::given(method("GET"))
        .and(path("/simple/velodexpkg/"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&h.server)
        .await;
    upload_wheel(&h.state, "velodexpkg-1.0-py3-none-any.whl", &fixture_wheel()).await;
    // Yank through the overlay: the uploaded file is rewritten, no override is created.
    assert_eq!(
        request(&h.state, "PUT", "/root/pypi/velodexpkg/1.0/yank", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (_, _, detail) = get(&h.state, "/root/pypi/simple/velodexpkg/", Some("application/json")).await;
    assert!(detail.contains("\"yanked\":true"));
    // A second identical yank changes nothing: uploaded state already matches, override skip too.
    let status = request(&h.state, "PUT", "/root/pypi/velodexpkg/1.0/yank", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Un-yank with no upstream override to clear only rewrites the record.
    assert_eq!(
        request(
            &h.state,
            "DELETE",
            "/root/pypi/velodexpkg/1.0/yank",
            Some(&upload_auth())
        )
        .await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn test_versioned_delete_matches_upload_record_when_filename_lacks_version() {
    let h = harness().await;
    // The filename carries no parsable version, so the served-page filter misses it and the
    // record-based fallback deletes by the version stored at upload time.
    let fields = vec![(":action", "file_upload"), ("name", "velodexpkg"), ("version", "9.9")];
    let (ct, body) = multipart_body(&fields, Some(("velodexpkg.whl", b"payload")));
    assert_eq!(
        post_upload(&h.state, "/local/", Some(&upload_auth()), &ct, body).await,
        StatusCode::OK
    );
    assert_eq!(
        request(&h.state, "DELETE", "/local/velodexpkg/9.9/", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (status, ..) = get(&h.state, "/local/simple/velodexpkg/", Some("application/json")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_versioned_delete_fallback_skips_other_versions() {
    let h = harness().await;
    // Neither filename carries a parsable version, so both deletes go through the record fallback.
    for (version, filename) in [("1.5", "velodexpkg-one.whl"), ("2.5", "velodexpkg-two.whl")] {
        let fields = vec![(":action", "file_upload"), ("name", "velodexpkg"), ("version", version)];
        let (ct, body) = multipart_body(&fields, Some((filename, format!("payload {version}").as_bytes())));
        assert_eq!(
            post_upload(&h.state, "/local/", Some(&upload_auth()), &ct, body).await,
            StatusCode::OK
        );
    }
    assert_eq!(
        request(&h.state, "DELETE", "/local/velodexpkg/1.5/", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (_, _, detail) = get(&h.state, "/local/simple/velodexpkg/", Some("application/json")).await;
    assert!(detail.contains("velodexpkg-two.whl"));
    assert!(!detail.contains("velodexpkg-one.whl"));
}

#[tokio::test]
async fn test_restore_skips_yanked_overrides_and_other_versions() {
    let h = harness().await;
    h.state
        .meta
        .put_override("local", "flask", "flask-1.0-py3-none-any.whl", "yanked")
        .unwrap();
    h.state
        .meta
        .put_override("local", "flask", "flask-2.0-py3-none-any.whl", "hidden")
        .unwrap();
    // Restoring 1.0 touches nothing: its override is a yank, and the hidden file is another version.
    let status = request(&h.state, "PUT", "/root/pypi/flask/1.0/restore", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // The hidden 2.0 override is still there to restore.
    let status = request(&h.state, "PUT", "/root/pypi/flask/2.0/restore", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn test_yank_with_corrupt_record_is_server_error() {
    let h = harness().await;
    h.state
        .meta
        .put_upload("local", "velodexpkg", "velodexpkg-1.0.whl", b"{ not json")
        .unwrap();
    let status = request(&h.state, "PUT", "/local/velodexpkg/1.0/yank", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn test_overlay_without_upload_layer_serves_merged_page() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let meta = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let upstream = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let digest = Digest::of(b"wheel");
    mount_detail(&server, digest.as_str(), "http://x/flask-1.0-py3-none-any.whl", None).await;
    let indexes = vec![
        Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            kind: IndexKind::Mirror(upstream),
        },
        Index {
            name: "ov".to_owned(),
            route: "ov".to_owned(),
            kind: IndexKind::Overlay {
                layers: vec![0],
                upload: None,
            },
        },
    ];
    let state = Arc::new(AppState::new(meta, blobs, 60, indexes));
    let (status, _, body) = get(&state, "/ov/simple/flask/", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("flask-1.0-py3-none-any.whl"));
}
