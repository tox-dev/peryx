use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use http_body_util::BodyExt as _;
use sha2::{Digest as _, Sha256};
use tower::ServiceExt as _;
use velodex_core::pypi::{CoreMetadata, File, Provenance, Yanked, to_json};
use velodex_storage::blob::{BlobStore, Digest};
use velodex_storage::meta::{CachedIndex, MetaStore};
use velodex_upstream::{Auth, UpstreamClient};
use wiremock::matchers::{header as match_header, header_regex, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{LogCapture, field};
use crate::path_safety::local_file_url;
use crate::router;
use crate::state::{AppState, Index, IndexKind};
use crate::upload::Uploaded;

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

async fn promotion_harness() -> Harness {
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
            name: "staging".to_owned(),
            route: "staging".to_owned(),
            kind: IndexKind::Local {
                upload_token: Some("s3cret".to_owned()),
                volatile: true,
            },
        },
        Index {
            name: "prod".to_owned(),
            route: "prod".to_owned(),
            kind: IndexKind::Local {
                upload_token: Some("s3cret".to_owned()),
                volatile: true,
            },
        },
        Index {
            name: "release".to_owned(),
            route: "release".to_owned(),
            kind: IndexKind::Overlay {
                layers: vec![2, 0],
                upload: Some(2),
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

fn put_raw_project_status(path: &Path, key: &str, value: &[u8]) {
    let db = redb::Database::create(path).unwrap();
    let table: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("project_status");
    let txn = db.begin_write().unwrap();
    txn.open_table(table).unwrap().insert(key, value).unwrap();
    txn.commit().unwrap();
}

pub(super) async fn get(state: &Arc<AppState>, uri: &str, accept: Option<&str>) -> (StatusCode, HeaderMap, String) {
    let (status, headers, bytes) = get_bytes(state, uri, accept).await;
    (status, headers, String::from_utf8_lossy(&bytes).into_owned())
}

async fn get_with_headers(state: &Arc<AppState>, uri: &str, extra_headers: &[(&str, &str)]) -> (StatusCode, String) {
    let mut builder = Request::builder().uri(uri).method("GET");
    for (name, value) in extra_headers {
        builder = builder.header(*name, *value);
    }
    let response = router(state.clone())
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn get_bytes(state: &Arc<AppState>, uri: &str, accept: Option<&str>) -> (StatusCode, HeaderMap, Vec<u8>) {
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
    (status, headers, bytes.to_vec())
}

pub(super) async fn request(state: &Arc<AppState>, verb: &str, uri: &str, auth: Option<&str>) -> StatusCode {
    request_response(state, verb, uri, auth).await.0
}

async fn request_response(state: &Arc<AppState>, verb: &str, uri: &str, auth: Option<&str>) -> (StatusCode, String) {
    let mut builder = Request::builder().uri(uri).method(verb);
    if let Some(auth) = auth {
        builder = builder.header(header::AUTHORIZATION, auth);
    }
    let response = router(state.clone())
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
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

async fn mount_status_detail(
    server: &MockServer,
    project: &str,
    status: &str,
    reason: &str,
    digest: &str,
    file_url: &str,
) {
    let body = format!(
        "{{\"meta\":{{\"api-version\":\"1.4\",\"project-status\":\"{status}\",\
         \"project-status-reason\":\"{reason}\"}},\"name\":\"{project}\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"{project}-1.0-py3-none-any.whl\",\"url\":\"{file_url}\",\
         \"hashes\":{{\"sha256\":\"{digest}\"}}}}]}}"
    );
    Mock::given(method("GET"))
        .and(path(format!("/simple/{project}/")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.into_bytes(), "application/vnd.pypi.simple.v1+json"))
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
async fn test_mirror_detail_json_preserves_simple_api_fields() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let meta_digest = Digest::of(b"meta");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    let json = format!(
        "{{\"meta\":{{\"api-version\":\"1.4\",\"project-status\":\"archived\",\
         \"project-status-reason\":\"read only\"}},\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\"url\":\"{file_url}\",\
         \"hashes\":{{\"sha256\":\"{digest}\"}},\"size\":123,\"upload-time\":\"2024-01-01T00:00:00Z\",\
         \"core-metadata\":{{\"sha256\":\"{meta}\"}},\"dist-info-metadata\":{{\"sha256\":\"{meta}\"}},\
         \"gpg-sig\":false,\"provenance\":\"https://example.test/flask.provenance\"}}]}}",
        digest = digest.as_str(),
        meta = meta_digest.as_str(),
    );
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(json.into_bytes(), "application/vnd.pypi.simple.v1+json"))
        .mount(&h.server)
        .await;

    let (status, _, body) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    let detail: serde_json::Value = serde_json::from_str(&body).unwrap();
    let file = &detail["files"][0];
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["meta"]["api-version"], "1.4");
    assert_eq!(detail["meta"]["project-status"], "archived");
    assert_eq!(detail["meta"]["project-status-reason"], "read only");
    assert_eq!(detail["versions"], serde_json::json!(["1.0"]));
    assert_eq!(file["size"], 123);
    assert_eq!(file["upload-time"], "2024-01-01T00:00:00Z");
    assert_eq!(file["core-metadata"]["sha256"], meta_digest.as_str());
    assert_eq!(file["dist-info-metadata"]["sha256"], meta_digest.as_str());
    assert_eq!(file["gpg-sig"], false);
    assert_eq!(file["provenance"], "https://example.test/flask.provenance");
    assert!(file["url"].as_str().unwrap().starts_with("/pypi/files/"));
}

#[tokio::test]
async fn test_legacy_project_json_serves_releases_from_simple_detail() {
    let h = harness().await;
    let wheel_digest = Digest::of(b"wheel");
    let sdist_digest = Digest::of(b"sdist");
    let wheel_url = format!("{}/files/flask-2.0.whl", h.server.uri());
    let sdist_url = format!("{}/files/flask-1.0.tar.gz", h.server.uri());
    let json = format!(
        "{{\"meta\":{{\"api-version\":\"1.4\"}},\"name\":\"flask\",\"versions\":[\"1.0\",\"2.0\"],\
         \"files\":[{{\"filename\":\"flask-2.0-py3-none-any.whl\",\"url\":\"{wheel_url}\",\
         \"hashes\":{{\"sha256\":\"{wheel_digest}\"}},\"requires-python\":\">=3.9\",\
         \"size\":123,\"upload-time\":\"2026-01-01T00:00:00.123456Z\"}},\
         {{\"filename\":\"flask-1.0.tar.gz\",\"url\":\"{sdist_url}\",\
         \"hashes\":{{\"sha256\":\"{sdist_digest}\"}},\"yanked\":\"bad build\",\
         \"size\":456,\"upload-time\":\"2025-12-31T23:59:59Z\"}}]}}",
        wheel_digest = wheel_digest.as_str(),
        sdist_digest = sdist_digest.as_str(),
    );
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(json.into_bytes(), "application/vnd.pypi.simple.v1+json"))
        .mount(&h.server)
        .await;

    let (status, headers, body) = get(&h.state, "/root/pypi/flask/json/", None).await;

    let legacy: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "application/json");
    assert_eq!(
        legacy["info"],
        serde_json::json!({
            "author": "",
            "author_email": "",
            "bugtrack_url": null,
            "classifiers": [],
            "description": "",
            "description_content_type": null,
            "docs_url": null,
            "download_url": "",
            "downloads": {"last_day": -1, "last_month": -1, "last_week": -1},
            "dynamic": [],
            "home_page": "",
            "keywords": "",
            "license": "",
            "license_expression": null,
            "license_files": null,
            "maintainer": "",
            "maintainer_email": "",
            "name": "flask",
            "package_url": "",
            "platform": null,
            "project_url": "",
            "project_urls": {},
            "provides_extra": [],
            "release_url": "",
            "requires_dist": [],
            "requires_python": ">=3.9",
            "summary": "",
            "version": "2.0",
            "yanked": false,
            "yanked_reason": null
        })
    );
    assert_eq!(legacy["urls"], legacy["releases"]["2.0"]);
    assert_eq!(
        legacy["urls"][0],
        serde_json::json!({
            "comment_text": "",
            "digests": {"sha256": wheel_digest.as_str()},
            "downloads": -1,
            "filename": "flask-2.0-py3-none-any.whl",
            "has_sig": false,
            "md5_digest": null,
            "packagetype": "bdist_wheel",
            "python_version": "py3",
            "requires_python": ">=3.9",
            "size": 123,
            "upload_time": "2026-01-01T00:00:00",
            "upload_time_iso_8601": "2026-01-01T00:00:00.123456Z",
            "url": format!("/root/pypi/files/{}/flask-2.0-py3-none-any.whl", wheel_digest.as_str()),
            "yanked": false,
            "yanked_reason": null
        })
    );
    assert_eq!(
        legacy["releases"]["1.0"][0]["url"],
        format!("/root/pypi/files/{}/flask-1.0.tar.gz", sdist_digest.as_str())
    );
    assert_eq!(legacy["vulnerabilities"], serde_json::json!([]));
    assert_eq!(
        legacy["ownership"],
        serde_json::json!({"roles": [], "organization": null})
    );
}

#[tokio::test]
async fn test_legacy_release_json_serves_one_version_without_releases() {
    let h = harness().await;
    let digest = Digest::of(b"sdist");
    let file_url = format!("{}/files/flask-1.0.tar.gz", h.server.uri());
    let json = format!(
        "{{\"meta\":{{\"api-version\":\"1.4\"}},\"name\":\"flask\",\"versions\":[\"1.0\",\"2.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0.tar.gz\",\"url\":\"{file_url}\",\
         \"hashes\":{{\"sha256\":\"{digest}\"}},\"yanked\":\"bad build\",\
         \"size\":456,\"upload-time\":\"2025-12-31T23:59:59Z\"}}]}}",
        digest = digest.as_str(),
    );
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(json.into_bytes(), "application/vnd.pypi.simple.v1+json"))
        .mount(&h.server)
        .await;

    let (status, _, body) = get(&h.state, "/root/pypi/flask/1.0/json", None).await;

    let legacy: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(legacy.get("releases"), None);
    assert_eq!(legacy["info"]["version"], "1.0");
    assert_eq!(legacy["info"]["yanked"], true);
    assert_eq!(legacy["info"]["yanked_reason"], "bad build");
    assert_eq!(
        legacy["urls"],
        serde_json::json!([{
            "comment_text": "",
            "digests": {"sha256": digest.as_str()},
            "downloads": -1,
            "filename": "flask-1.0.tar.gz",
            "has_sig": false,
            "md5_digest": null,
            "packagetype": "sdist",
            "python_version": "source",
            "requires_python": null,
            "size": 456,
            "upload_time": "2025-12-31T23:59:59",
            "upload_time_iso_8601": "2025-12-31T23:59:59Z",
            "url": format!("/root/pypi/files/{}/flask-1.0.tar.gz", digest.as_str()),
            "yanked": true,
            "yanked_reason": "bad build"
        }])
    );
}

#[tokio::test]
async fn test_legacy_release_json_unknown_version_is_not_found() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    mount_detail(&h.server, digest.as_str(), "http://x/flask-1.0-py3-none-any.whl", None).await;

    let (status, _, body) = get(&h.state, "/pypi/flask/9.9/json", None).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("version Some(\"9.9\") was not found"));
}

#[tokio::test]
async fn test_legacy_json_rejects_invalid_project_path() {
    let h = harness().await;

    let (status, _, body) = get(&h.state, "/pypi/%FF/json", None).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("invalid percent-encoded path segment"));
}

#[tokio::test]
async fn test_legacy_json_rejects_invalid_versioned_project_path() {
    let h = harness().await;

    let (status, _, body) = get(&h.state, "/pypi/%FF/1.0/json", None).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("invalid percent-encoded path segment"));
}

#[tokio::test]
async fn test_legacy_json_rejects_invalid_version_path() {
    let h = harness().await;

    let (status, _, body) = get(&h.state, "/pypi/flask/%FF/json", None).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("invalid percent-encoded path segment"));
}

#[tokio::test]
async fn test_legacy_json_rejects_unsafe_project_path() {
    let h = harness().await;

    let (status, _, body) = get(&h.state, "/pypi/flask%2Fbad/json", None).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("invalid project \"flask/bad\""));
}

#[tokio::test]
async fn test_legacy_json_rejects_unsafe_versioned_project_path() {
    let h = harness().await;

    let (status, _, body) = get(&h.state, "/pypi/flask%2Fbad/1.0/json", None).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("invalid project \"flask/bad\""));
}

#[tokio::test]
async fn test_legacy_json_rejects_unsafe_version_path() {
    let h = harness().await;

    let (status, _, body) = get(&h.state, "/pypi/flask/1.0%2Fbad/json", None).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("invalid version \"1.0/bad\""));
}

#[tokio::test]
async fn test_legacy_json_missing_project_is_not_found() {
    let h = harness().await;
    Mock::given(method("GET"))
        .and(path("/simple/missing/"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&h.server)
        .await;

    let (status, _, body) = get(&h.state, "/pypi/missing/json", None).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.contains("project \"missing\" was not found on index \"pypi\""));
}

#[tokio::test]
async fn test_legacy_json_unsupported_upstream_content_type_is_bad_gateway() {
    let h = harness().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(b"not an index".to_vec(), "application/octet-stream"))
        .mount(&h.server)
        .await;

    let (status, _, body) = get(&h.state, "/pypi/flask/json", None).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains("unsupported upstream Simple API Content-Type"));
}

#[tokio::test]
async fn test_legacy_json_unavailable_upstream_is_bad_gateway() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let upstream = UpstreamClient::new("http://127.0.0.1:0/simple/").unwrap();
    let state = Arc::new(AppState::new(
        meta,
        blobs,
        60,
        vec![Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            kind: IndexKind::Mirror(upstream),
        }],
    ));

    let (status, _, body) = get(&state, "/pypi/flask/json", None).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains("project detail on index \"pypi\" for project \"flask\""));
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
async fn test_mirror_detail_from_html_preserves_simple_api_fields() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let metadata = Digest::of(b"meta");
    let html = format!(
        r#"<!DOCTYPE html><html><head>
        <meta name="pypi:repository-version" content="1.4">
        <meta name="pypi:project-status" content="archived">
        <meta name="pypi:project-status-reason" content="read only">
        </head><body>
        <a href="/files/flask.whl#sha256={digest}" data-core-metadata="sha256={metadata}"
           data-dist-info-metadata="sha256={metadata}" data-gpg-sig="true"
           data-provenance="https://example.test/flask.provenance">flask-1.0.whl</a>
        </body></html>"#,
        digest = digest.as_str(),
        metadata = metadata.as_str(),
    );
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(html.into_bytes(), "text/html"))
        .mount(&h.server)
        .await;

    let (status, _, body) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    let detail: serde_json::Value = serde_json::from_str(&body).unwrap();
    let file = &detail["files"][0];
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["meta"]["project-status"], "archived");
    assert_eq!(detail["meta"]["project-status-reason"], "read only");
    assert_eq!(file["core-metadata"]["sha256"], metadata.as_str());
    assert_eq!(file["dist-info-metadata"]["sha256"], metadata.as_str());
    assert_eq!(file["gpg-sig"], true);
    assert_eq!(file["provenance"], "https://example.test/flask.provenance");
}

#[tokio::test]
async fn test_unsupported_simple_api_major_version_is_bad_gateway() {
    let h = harness().await;
    let json = r#"{"name":"flask","meta":{"api-version":"2.0"},"versions":[],"files":[]}"#;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(json.as_bytes().to_vec(), "application/vnd.pypi.simple.v1+json"),
        )
        .mount(&h.server)
        .await;

    let (status, _, body) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains("project detail on index \"pypi\" for project \"flask\""));
    assert!(body.contains("unsupported upstream Simple API version \"2.0\""));
}

#[tokio::test]
async fn test_unsupported_upstream_content_type_is_bad_gateway() {
    let h = harness().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(b"not an index".to_vec(), "application/octet-stream"))
        .mount(&h.server)
        .await;

    let (status, _, body) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains("unsupported upstream Simple API Content-Type"));
    assert!(body.contains("/simple/flask/"));
}

#[tokio::test]
async fn test_unknown_route_is_not_found() {
    let h = harness().await;
    let (status, ..) = get(&h.state, "/nope/simple/flask/", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_discovery_document_uses_request_origin_and_redacts_token() {
    let h = harness().await;
    let (status, body) = get_with_headers(
        &h.state,
        "/+api",
        &[
            ("host", "internal.local"),
            ("x-forwarded-host", "packages.example"),
            ("x-forwarded-proto", "https"),
        ],
    )
    .await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    let indexes = json["indexes"].as_array().unwrap();
    let overlay = indexes.iter().find(|index| index["route"] == "root/pypi").unwrap();
    let mirror = indexes.iter().find(|index| index["route"] == "pypi").unwrap();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json["urls"],
        serde_json::json!({
            "api": "https://packages.example/+api",
            "status": "https://packages.example/+status",
            "stats": "https://packages.example/+stats",
            "openapi": "https://packages.example/api-docs/openapi.json",
            "web": "https://packages.example/"
        })
    );
    assert_eq!(
        overlay["urls"],
        serde_json::json!({
            "api": "https://packages.example/root/pypi/+api",
            "simple": "https://packages.example/root/pypi/simple/",
            "upload": "https://packages.example/root/pypi/",
            "status": "https://packages.example/+status",
            "web": "https://packages.example/browse?index=root%2Fpypi",
            "stats": "https://packages.example/stats?index=root%2Fpypi",
            "openapi": "https://packages.example/api-docs/openapi.json"
        })
    );
    assert_eq!(
        overlay["capabilities"],
        serde_json::json!({
            "simple_html": true,
            "simple_json": true,
            "simple_api_version": "1.4",
            "metadata_siblings": true,
            "uploads": true,
            "yanking": true,
            "volatile_deletes": true,
            "project_status": true,
            "provenance": true,
            "legacy_json": true
        })
    );
    assert_eq!(mirror["urls"].get("upload"), None);
    assert_eq!(mirror["client_configuration"].get(".pypirc"), None);
    assert_eq!(mirror["capabilities"]["uploads"], false);
    assert_eq!(mirror["capabilities"]["yanking"], false);
    assert_eq!(mirror["capabilities"]["volatile_deletes"], false);
    assert!(body.contains("\"uv.toml\""));
    assert!(body.contains("password = <upload-token>"));
    assert!(!body.contains("s3cret"));
}

#[tokio::test]
async fn test_index_discovery_route_accepts_trailing_slash() {
    let h = harness().await;
    let (status, body) = get_with_headers(&h.state, "/root/pypi/+api/", &[("host", "127.0.0.1:4433")]).await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["index"]["route"], "root/pypi");
    assert_eq!(
        json["index"]["urls"]["simple"],
        "http://127.0.0.1:4433/root/pypi/simple/"
    );
}

#[tokio::test]
async fn test_index_discovery_unknown_route_is_not_found() {
    let h = harness().await;
    let (status, ..) = get(&h.state, "/missing/+api", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_mirror_detail_upstream_404() {
    let h = harness().await;
    Mock::given(method("GET"))
        .and(path("/simple/missing/"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&h.server)
        .await;
    let (status, ..) = get(&h.state, "/pypi/simple/missing/", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
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
async fn test_quarantined_project_hides_files_and_blocks_downloads() {
    let h = harness().await;
    let wheel = b"wheelcontent";
    let digest = Digest::of(wheel);
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_detail(&h.server, digest.as_str(), &file_url, Some("\"active\"")).await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    h.server.reset().await;
    mount_status_detail(&h.server, "flask", "quarantined", "malware", digest.as_str(), &file_url).await;
    h.clock.store(5000, Ordering::Relaxed);

    let (status, _, detail) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["meta"]["project-status"], "quarantined");
    assert!(detail["files"].as_array().unwrap().is_empty());

    let uri = format!("/pypi/files/{}/flask-1.0-py3-none-any.whl", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        body,
        "project for file \"flask-1.0-py3-none-any.whl\" is quarantined; downloads are disabled"
    );

    let overlay_uri = format!("/root/pypi/files/{}/flask-1.0-py3-none-any.whl", digest.as_str());
    let (status, _, body) = get(&h.state, &overlay_uri, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        body,
        "project for file \"flask-1.0-py3-none-any.whl\" is quarantined; downloads are disabled"
    );
}

#[tokio::test]
async fn test_file_download_status_store_error_is_server_error() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("velodex.redb");
    MetaStore::open(&db_path).unwrap();
    put_raw_project_status(&db_path, "pypi/flask", b"not json");
    let meta = MetaStore::open(&db_path).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let upstream = UpstreamClient::new("http://127.0.0.1:0/simple/").unwrap();
    let indexes = vec![Index {
        name: "pypi".to_owned(),
        route: "pypi".to_owned(),
        kind: IndexKind::Mirror(upstream),
    }];
    let state = Arc::new(AppState::new(meta, blobs, 60, indexes));

    let uri = format!(
        "/pypi/files/{}/flask-1.0-py3-none-any.whl",
        Digest::of(b"wheel").as_str()
    );
    let (status, _, body) = get(&state, &uri, None).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body.contains("file download on index \"pypi\""));
    assert!(body.contains("metadata store error"));
}

#[tokio::test]
async fn test_file_download_invalid_digest_is_bad_request() {
    let h = harness().await;
    let (status, _, body) = get(&h.state, "/pypi/files/notahex/x.whl", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("expected 64 lowercase hex sha256"));
}

#[tokio::test]
async fn test_file_download_rejects_encoded_path_filename() {
    let h = harness().await;
    let uri = format!("/pypi/files/{}/pkg%2Fname.whl", "a".repeat(64));
    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("filenames must be relative path segments"));
}

#[tokio::test]
async fn test_file_download_allows_literal_percent_filename() {
    let h = harness().await;
    let digest = put_local_file(&h.state, "velodexpkg%2F.whl", b"PKpercent", "1.0");
    let uri = format!("/local/files/{}/velodexpkg%252F.whl", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "PKpercent");
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

#[tokio::test]
async fn test_metadata_backfill_reads_wheel_ranges() {
    let h = harness().await;
    let metadata = b"Metadata-Version: 2.1\nName: velodexpkg\nVersion: 1.0\n";
    let wheel = fixture_wheel_with_metadata(metadata);
    let digest = Digest::of(&wheel);
    let filename = "velodexpkg-1.0-py3-none-any.whl";
    let file_url = format!("{}/files/{filename}", h.server.uri());
    h.state.meta.put_file_url(digest.as_str(), &file_url, "pypi").unwrap();
    Mock::given(method("HEAD"))
        .and(path(format!("/files/{filename}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("accept-ranges", "bytes")
                .insert_header("content-length", wheel.len()),
        )
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{filename}")))
        .and(match_header("accept-encoding", "identity"))
        .respond_with(range_response(wheel))
        .mount(&h.server)
        .await;

    let uri = format!("/pypi/files/{}/{filename}.metadata", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_bytes(), metadata);
    let (url, metadata_sha256, source) = h
        .state
        .meta
        .get_metadata(digest.as_str())
        .unwrap()
        .expect("generated metadata registered");
    assert_eq!(url, "velodex:generated");
    assert_eq!(metadata_sha256, Digest::of(metadata).as_str());
    assert_eq!(source, "pypi");
}

#[tokio::test]
async fn test_metadata_backfill_upstream_range_error_is_bad_gateway() {
    let h = harness().await;
    let wheel = fixture_wheel_with_metadata(b"Metadata-Version: 2.1\nName: velodexpkg\nVersion: 1.0\n");
    let digest = Digest::of(&wheel);
    let filename = "velodexpkg-1.0-py3-none-any.whl";
    h.state
        .meta
        .put_file_url(digest.as_str(), &format!("{}/files/{filename}", h.server.uri()), "pypi")
        .unwrap();
    Mock::given(method("HEAD"))
        .and(path(format!("/files/{filename}")))
        .respond_with(ResponseTemplate::new(500))
        .mount(&h.server)
        .await;

    let uri = format!("/pypi/files/{}/{filename}.metadata", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains("upstream returned 500 Internal Server Error"));
}

#[tokio::test]
async fn test_metadata_backfill_reads_cached_wheel_blob() {
    let h = harness().await;
    let metadata = b"Metadata-Version: 2.1\nName: velodexpkg\nVersion: 1.0\n";
    let wheel = fixture_wheel_with_metadata(metadata);
    let digest = h.state.blobs.write(&wheel).unwrap();
    let filename = "velodexpkg-1.0-py3-none-any.whl";
    h.state
        .meta
        .put_file_url(digest.as_str(), &format!("{}/files/{filename}", h.server.uri()), "pypi")
        .unwrap();

    let uri = format!("/pypi/files/{}/{filename}.metadata", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_bytes(), metadata);
}

#[tokio::test]
async fn test_metadata_backfill_downloads_when_ranges_fail() {
    let h = harness().await;
    let metadata = b"Metadata-Version: 2.1\nName: velodexpkg\nVersion: 1.0\n";
    let wheel = fixture_wheel_with_metadata(metadata);
    let digest = Digest::of(&wheel);
    let filename = "velodexpkg-1.0-py3-none-any.whl";
    let file_url = format!("{}/files/{filename}", h.server.uri());
    h.state.meta.put_file_url(digest.as_str(), &file_url, "pypi").unwrap();
    Mock::given(method("HEAD"))
        .and(path(format!("/files/{filename}")))
        .respond_with(ResponseTemplate::new(405))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{filename}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(wheel))
        .mount(&h.server)
        .await;

    let uri = format!("/pypi/files/{}/{filename}.metadata", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_bytes(), metadata);
    assert!(h.state.blobs.exists(&digest));
}

#[tokio::test]
async fn test_metadata_backfill_downloads_sdist_without_ranges() {
    let h = harness().await;
    let sdist = fixture_sdist();
    let digest = Digest::of(&sdist);
    let filename = "velodexpkg-1.0.tar.gz";
    h.state
        .meta
        .put_file_url(digest.as_str(), &format!("{}/files/{filename}", h.server.uri()), "pypi")
        .unwrap();
    Mock::given(method("GET"))
        .and(path(format!("/files/{filename}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(sdist))
        .mount(&h.server)
        .await;

    let uri = format!("/pypi/files/{}/{filename}.metadata", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "Metadata-Version: 2.2\nName: velodexpkg\nVersion: 1.0\n");
}

#[tokio::test]
async fn test_metadata_backfill_missing_wheel_metadata_is_not_found() {
    let h = harness().await;
    let wheel = fixture_wheel_without_metadata();
    let digest = Digest::of(&wheel);
    let filename = "velodexpkg-1.0-py3-none-any.whl";
    h.state
        .meta
        .put_file_url(digest.as_str(), &format!("{}/files/{filename}", h.server.uri()), "pypi")
        .unwrap();
    Mock::given(method("HEAD"))
        .and(path(format!("/files/{filename}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("accept-ranges", "bytes")
                .insert_header("content-length", wheel.len()),
        )
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{filename}")))
        .and(header_regex("range", "^bytes=[0-9]+-[0-9]+$"))
        .respond_with(range_response(wheel))
        .mount(&h.server)
        .await;

    let uri = format!("/pypi/files/{}/{filename}.metadata", digest.as_str());
    let (status, ..) = get(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_metadata_backfill_downloads_when_range_zip_is_unsupported() {
    let h = harness().await;
    let metadata = b"Metadata-Version: 2.1\nName: velodexpkg\nVersion: 1.0\n";
    let wheel = fixture_wheel_with_metadata(metadata);
    let digest = Digest::of(&wheel);
    let filename = "velodexpkg-1.0-py3-none-any.whl";
    h.state
        .meta
        .put_file_url(digest.as_str(), &format!("{}/files/{filename}", h.server.uri()), "pypi")
        .unwrap();
    Mock::given(method("HEAD"))
        .and(path(format!("/files/{filename}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("accept-ranges", "bytes")
                .insert_header("content-length", "0"),
        )
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{filename}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(wheel))
        .mount(&h.server)
        .await;

    let uri = format!("/pypi/files/{}/{filename}.metadata", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_bytes(), metadata);
}

#[tokio::test]
async fn test_metadata_backfill_downloads_when_range_tail_is_not_zip() {
    let h = harness().await;
    let metadata = b"Metadata-Version: 2.1\nName: velodexpkg\nVersion: 1.0\n";
    let wheel = fixture_wheel_with_metadata(metadata);

    assert_metadata_range_fallback(&h, vec![0; 128], wheel, metadata).await;
}

#[tokio::test]
async fn test_metadata_backfill_downloads_when_range_directory_is_empty() {
    let h = harness().await;
    let metadata = b"Metadata-Version: 2.1\nName: velodexpkg\nVersion: 1.0\n";
    let wheel = fixture_wheel_with_metadata(metadata);

    assert_metadata_range_fallback(&h, empty_zip(), wheel, metadata).await;
}

#[tokio::test]
async fn test_metadata_backfill_downloads_when_range_directory_is_invalid() {
    let h = harness().await;
    let metadata = b"Metadata-Version: 2.1\nName: velodexpkg\nVersion: 1.0\n";
    let wheel = fixture_wheel_with_metadata(metadata);
    let mut ranged = wheel.clone();
    overwrite_metadata_central_signature(&mut ranged, [0, 0, 0, 0]);

    assert_metadata_range_fallback(&h, ranged, wheel, metadata).await;
}

#[tokio::test]
async fn test_metadata_backfill_downloads_when_range_metadata_is_too_large() {
    let h = harness().await;
    let metadata = b"Metadata-Version: 2.1\nName: velodexpkg\nVersion: 1.0\n";
    let wheel = fixture_wheel_with_metadata(metadata);
    let ranged = wheel_with_metadata_uncompressed_size(
        metadata,
        u32::try_from(crate::archive::MAX_WHEEL_METADATA_BYTES).unwrap() + 1,
    );

    assert_metadata_range_fallback(&h, ranged, wheel, metadata).await;
}

#[tokio::test]
async fn test_metadata_backfill_skips_ranges_after_disable() {
    let h = harness().await;
    let first = fixture_wheel_with_metadata(b"Metadata-Version: 2.1\nName: velodexpkg\nVersion: 1.0\n");
    let first_digest = Digest::of(&first);
    let first_filename = "velodexpkg-1.0-py3-none-any.whl";
    h.state
        .meta
        .put_file_url(
            first_digest.as_str(),
            &format!("{}/files/{first_filename}", h.server.uri()),
            "pypi",
        )
        .unwrap();
    Mock::given(method("HEAD"))
        .and(path(format!("/files/{first_filename}")))
        .respond_with(ResponseTemplate::new(405))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{first_filename}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(first))
        .mount(&h.server)
        .await;

    let first_uri = format!("/pypi/files/{}/{first_filename}.metadata", first_digest.as_str());
    assert_eq!(get(&h.state, &first_uri, None).await.0, StatusCode::OK);

    let second_metadata = b"Metadata-Version: 2.1\nName: velodexpkg\nVersion: 2.0\n";
    let second = fixture_wheel_with_body_and_metadata("2.0", b"VALUE = 2\n", Some(second_metadata));
    let second_digest = Digest::of(&second);
    let second_filename = "velodexpkg-2.0-py3-none-any.whl";
    h.state
        .meta
        .put_file_url(
            second_digest.as_str(),
            &format!("{}/files/{second_filename}", h.server.uri()),
            "pypi",
        )
        .unwrap();
    Mock::given(method("GET"))
        .and(path(format!("/files/{second_filename}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(second))
        .mount(&h.server)
        .await;

    let second_uri = format!("/pypi/files/{}/{second_filename}.metadata", second_digest.as_str());
    let (status, _, body) = get(&h.state, &second_uri, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_bytes(), second_metadata);
}

#[tokio::test]
async fn test_metadata_backfill_downloads_when_range_deflate_is_invalid() {
    let h = harness().await;
    let metadata = b"Metadata-Version: 2.1\nName: velodexpkg\nVersion: 1.0\n";
    let wheel = fixture_wheel_with_metadata(metadata);
    let ranged = wheel_with_invalid_deflated_metadata(metadata);
    let digest = Digest::of(&wheel);
    let filename = "velodexpkg-1.0-py3-none-any.whl";
    h.state
        .meta
        .put_file_url(digest.as_str(), &format!("{}/files/{filename}", h.server.uri()), "pypi")
        .unwrap();
    Mock::given(method("HEAD"))
        .and(path(format!("/files/{filename}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("accept-ranges", "bytes")
                .insert_header("content-length", ranged.len()),
        )
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{filename}")))
        .and(header_regex("range", "^bytes=[0-9]+-[0-9]+$"))
        .respond_with(range_response(ranged))
        .with_priority(1)
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{filename}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(wheel))
        .with_priority(10)
        .mount(&h.server)
        .await;

    let uri = format!("/pypi/files/{}/{filename}.metadata", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_bytes(), metadata);
}

#[tokio::test]
async fn test_metadata_backfill_downloads_when_range_compression_is_unsupported() {
    let h = harness().await;
    let metadata = b"Metadata-Version: 2.1\nName: velodexpkg\nVersion: 1.0\n";
    let wheel = fixture_wheel_with_metadata(metadata);
    let ranged = wheel_with_metadata_compression_method(metadata, 99);
    let digest = Digest::of(&wheel);
    let filename = "velodexpkg-1.0-py3-none-any.whl";
    h.state
        .meta
        .put_file_url(digest.as_str(), &format!("{}/files/{filename}", h.server.uri()), "pypi")
        .unwrap();
    Mock::given(method("HEAD"))
        .and(path(format!("/files/{filename}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("accept-ranges", "bytes")
                .insert_header("content-length", ranged.len()),
        )
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{filename}")))
        .and(header_regex("range", "^bytes=[0-9]+-[0-9]+$"))
        .respond_with(range_response(ranged))
        .with_priority(1)
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{filename}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(wheel))
        .with_priority(10)
        .mount(&h.server)
        .await;

    let uri = format!("/pypi/files/{}/{filename}.metadata", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_bytes(), metadata);
}

#[tokio::test]
async fn test_metadata_backfill_downloads_when_range_size_mismatches() {
    let h = harness().await;
    let metadata = b"Metadata-Version: 2.1\nName: velodexpkg\nVersion: 1.0\n";
    let wheel = fixture_wheel_with_metadata(metadata);
    let ranged = wheel_with_metadata_uncompressed_size(metadata, u32::try_from(metadata.len()).unwrap() + 1);
    let digest = Digest::of(&wheel);
    let filename = "velodexpkg-1.0-py3-none-any.whl";
    h.state
        .meta
        .put_file_url(digest.as_str(), &format!("{}/files/{filename}", h.server.uri()), "pypi")
        .unwrap();
    Mock::given(method("HEAD"))
        .and(path(format!("/files/{filename}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("accept-ranges", "bytes")
                .insert_header("content-length", ranged.len()),
        )
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{filename}")))
        .and(header_regex("range", "^bytes=[0-9]+-[0-9]+$"))
        .respond_with(range_response(ranged))
        .with_priority(1)
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{filename}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(wheel))
        .with_priority(10)
        .mount(&h.server)
        .await;

    let uri = format!("/pypi/files/{}/{filename}.metadata", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_bytes(), metadata);
}

#[tokio::test]
async fn test_metadata_backfill_downloads_when_range_local_header_is_invalid() {
    let h = harness().await;
    let metadata = b"Metadata-Version: 2.1\nName: velodexpkg\nVersion: 1.0\n";
    let wheel = fixture_wheel_with_metadata(metadata);
    let mut ranged = wheel.clone();
    overwrite_metadata_local_signature(&mut ranged, [0, 0, 0, 0]);
    let digest = Digest::of(&wheel);
    let filename = "velodexpkg-1.0-py3-none-any.whl";
    h.state
        .meta
        .put_file_url(digest.as_str(), &format!("{}/files/{filename}", h.server.uri()), "pypi")
        .unwrap();
    Mock::given(method("HEAD"))
        .and(path(format!("/files/{filename}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("accept-ranges", "bytes")
                .insert_header("content-length", ranged.len()),
        )
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{filename}")))
        .and(header_regex("range", "^bytes=[0-9]+-[0-9]+$"))
        .respond_with(range_response(ranged))
        .with_priority(1)
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{filename}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(wheel))
        .with_priority(10)
        .mount(&h.server)
        .await;

    let uri = format!("/pypi/files/{}/{filename}.metadata", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_bytes(), metadata);
}

#[tokio::test]
async fn test_metadata_backfill_reads_empty_stored_range_metadata() {
    let h = harness().await;
    let wheel = fixture_wheel_with_metadata_compression(b"", zip::CompressionMethod::Stored);
    let digest = Digest::of(&wheel);
    let filename = "velodexpkg-1.0-py3-none-any.whl";
    h.state
        .meta
        .put_file_url(digest.as_str(), &format!("{}/files/{filename}", h.server.uri()), "pypi")
        .unwrap();
    Mock::given(method("HEAD"))
        .and(path(format!("/files/{filename}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("accept-ranges", "bytes")
                .insert_header("content-length", wheel.len()),
        )
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{filename}")))
        .and(header_regex("range", "^bytes=[0-9]+-[0-9]+$"))
        .respond_with(range_response(wheel))
        .mount(&h.server)
        .await;

    let uri = format!("/pypi/files/{}/{filename}.metadata", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.is_empty());
}

fn range_response(bytes: Vec<u8>) -> impl wiremock::Respond {
    move |request: &wiremock::Request| {
        let Some(range) = request
            .headers
            .get("range")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("bytes="))
        else {
            return ResponseTemplate::new(416);
        };
        let Some((start, end)) = range.split_once('-') else {
            return ResponseTemplate::new(416);
        };
        let (Some(start), Some(end)) = (start.parse::<usize>().ok(), end.parse::<usize>().ok()) else {
            return ResponseTemplate::new(416);
        };
        if start > end || end >= bytes.len() {
            return ResponseTemplate::new(416);
        }
        ResponseTemplate::new(206)
            .insert_header("accept-ranges", "bytes")
            .insert_header("content-range", format!("bytes {start}-{end}/{}", bytes.len()))
            .set_body_bytes(bytes[start..=end].to_vec())
    }
}

async fn assert_metadata_range_fallback(h: &Harness, ranged: Vec<u8>, wheel: Vec<u8>, metadata: &[u8]) {
    let digest = Digest::of(&wheel);
    let filename = "velodexpkg-1.0-py3-none-any.whl";
    h.state
        .meta
        .put_file_url(digest.as_str(), &format!("{}/files/{filename}", h.server.uri()), "pypi")
        .unwrap();
    Mock::given(method("HEAD"))
        .and(path(format!("/files/{filename}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("accept-ranges", "bytes")
                .insert_header("content-length", ranged.len()),
        )
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{filename}")))
        .and(header_regex("range", "^bytes=[0-9]+-[0-9]+$"))
        .respond_with(range_response(ranged))
        .with_priority(1)
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/files/{filename}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(wheel))
        .with_priority(10)
        .mount(&h.server)
        .await;

    let uri = format!("/pypi/files/{}/{filename}.metadata", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_bytes(), metadata);
}

fn upload_fields() -> Vec<(&'static str, &'static str)> {
    vec![
        (":action", "file_upload"),
        ("name", "velodexpkg"),
        ("version", "1.0"),
        ("filetype", "bdist_wheel"),
        ("requires_python", ">=3.8"),
    ]
}

pub(super) fn multipart_body(fields: &[(&str, &str)], content: Option<(&str, &[u8])>) -> (String, Vec<u8>) {
    let contents = content.into_iter().collect::<Vec<_>>();
    multipart_body_with_content_parts(fields, &contents)
}

fn multipart_body_with_content_parts(fields: &[(&str, &str)], contents: &[(&str, &[u8])]) -> (String, Vec<u8>) {
    let boundary = "velodextestboundary";
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes(),
        );
    }
    for (filename, bytes) in contents {
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

pub(super) fn upload_auth() -> String {
    format!("Basic {}", STANDARD.encode("__token__:s3cret"))
}

pub(super) async fn post_upload(
    state: &Arc<AppState>,
    uri: &str,
    auth: Option<&str>,
    content_type: &str,
    body: Vec<u8>,
) -> StatusCode {
    post_upload_response(state, uri, auth, content_type, body).await.0
}

pub(super) async fn post_upload_response(
    state: &Arc<AppState>,
    uri: &str,
    auth: Option<&str>,
    content_type: &str,
    body: Vec<u8>,
) -> (StatusCode, String) {
    let mut builder = Request::builder()
        .uri(uri)
        .method("POST")
        .header(header::CONTENT_TYPE, content_type);
    if let Some(auth) = auth {
        builder = builder.header(header::AUTHORIZATION, auth);
    }
    let response = router(state.clone())
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn assert_upload_response(
    h: &Harness,
    fields: &[(&str, &str)],
    content: Option<(&str, &[u8])>,
    expected_status: StatusCode,
    expected_body: &str,
) {
    let (ct, body) = multipart_body(fields, content);
    let (status, body) = post_upload_response(&h.state, "/root/pypi/", Some(&upload_auth()), &ct, body).await;
    assert_eq!(status, expected_status);
    assert_eq!(body, expected_body);
}

pub(super) async fn upload_velodexpkg(state: &Arc<AppState>, uri: &str, wheel: &[u8]) -> StatusCode {
    let (ct, body) = multipart_body(&upload_fields(), Some(("velodexpkg-1.0-py3-none-any.whl", wheel)));
    post_upload(state, uri, Some(&upload_auth()), &ct, body).await
}

#[tokio::test(flavor = "current_thread")]
async fn test_security_logs_upload_success_without_token_secret() {
    let h = harness().await;
    let logs = LogCapture::default();
    let guard = logs.install();

    assert_eq!(
        upload_velodexpkg(&h.state, "/root/pypi/", &fixture_wheel()).await,
        StatusCode::OK
    );

    drop(guard);
    let text = logs.text();
    assert!(!text.contains("s3cret"));
    let events = logs.security_events();
    assert!(events.iter().any(|event| {
        field(event, "action") == Some("token_use")
            && field(event, "result") == Some("success")
            && field(event, "actor") == Some("__token__")
            && field(event, "repository") == Some("local")
    }));
    let upload = events
        .iter()
        .find(|event| field(event, "action") == Some("upload") && field(event, "result") == Some("success"))
        .unwrap();
    assert_eq!(field(upload, "repository"), Some("root/pypi"));
    assert_eq!(field(upload, "local_repository"), Some("local"));
    assert_eq!(field(upload, "project"), Some("velodexpkg"));
    assert_eq!(field(upload, "version"), Some("1.0"));
    assert_eq!(field(upload, "filename"), Some("velodexpkg-1.0-py3-none-any.whl"));
    assert_eq!(upload["fields"]["count"], 1);
    assert!(field(upload, "digest").is_some_and(|digest| digest.len() == 64));
}

#[tokio::test(flavor = "current_thread")]
async fn test_security_logs_invalid_token_without_secret() {
    let h = harness().await;
    let (content_type, body) = multipart_body(&upload_fields(), Some(("velodexpkg-1.0-py3-none-any.whl", b"x")));
    let auth = format!("Basic {}", STANDARD.encode("alice:nope"));
    let logs = LogCapture::default();
    let guard = logs.install();

    assert_eq!(
        post_upload(&h.state, "/root/pypi/", Some(&auth), &content_type, body).await,
        StatusCode::UNAUTHORIZED
    );

    drop(guard);
    let text = logs.text();
    assert!(!text.contains("nope"));
    assert!(!text.contains("s3cret"));
    let events = logs.security_events();
    let token = events
        .iter()
        .find(|event| field(event, "action") == Some("token_use") && field(event, "result") == Some("denied"))
        .unwrap();
    assert_eq!(field(token, "actor"), Some("alice"));
    assert_eq!(field(token, "repository"), Some("local"));
    assert_eq!(field(token, "reason"), Some("invalid upload token"));
}

#[tokio::test]
async fn test_upload_via_overlay_then_serve_and_download() {
    let h = harness().await;
    let wheel = fixture_wheel();
    assert_eq!(upload_velodexpkg(&h.state, "/root/pypi/", &wheel).await, StatusCode::OK);

    // Served through the overlay, with the URL on the overlay route.
    let (ds, _, detail) = get(&h.state, "/root/pypi/simple/velodexpkg/", Some("application/json")).await;
    assert_eq!(ds, StatusCode::OK);
    assert!(detail.contains("velodexpkg-1.0-py3-none-any.whl"));
    assert!(detail.contains("\"1.0\""));
    let digest = Digest::of(&wheel);
    assert!(detail.contains(&format!("/root/pypi/files/{}/velodexpkg", digest.as_str())));

    let uri = format!("/root/pypi/files/{}/velodexpkg-1.0-py3-none-any.whl", digest.as_str());
    let (fs, _, fbody) = get_bytes(&h.state, &uri, None).await;
    assert_eq!(fs, StatusCode::OK);
    assert_eq!(fbody, wheel);

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
    upload_velodexpkg(&state, "/root/pypi/", &fixture_wheel()).await;
    // The mirror layer is unreachable, but the local layer still serves the upload.
    let (status, _, detail) = get(&state, "/root/pypi/simple/velodexpkg/", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(detail.contains("velodexpkg"));
}

#[tokio::test]
async fn test_upload_direct_to_local_route() {
    let h = harness().await;
    assert_eq!(
        upload_velodexpkg(&h.state, "/local/", &fixture_wheel()).await,
        StatusCode::OK
    );
    let (status, _, detail) = get(&h.state, "/local/simple/velodexpkg/", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(detail.contains("velodexpkg"));
}

#[tokio::test]
async fn test_upload_sdist_gains_metadata_sibling() {
    let h = harness().await;
    let sdist = fixture_sdist();
    let fields = vec![
        (":action", "file_upload"),
        ("name", "velodexpkg"),
        ("version", "1.0"),
        ("filetype", "sdist"),
    ];
    let (content_type, body) = multipart_body(&fields, Some(("velodexpkg-1.0.tar.gz", &sdist)));
    assert_eq!(
        post_upload(&h.state, "/local/", Some(&upload_auth()), &content_type, body).await,
        StatusCode::OK
    );

    let digest = Digest::of(&sdist);
    let (_, _, detail) = get(&h.state, "/local/simple/velodexpkg/", Some("application/json")).await;
    assert!(detail.contains("\"core-metadata\":{\"sha256\""));
    let (status, _, body) = get(
        &h.state,
        &format!("/local/files/{}/velodexpkg-1.0.tar.gz.metadata", digest.as_str()),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.starts_with("Metadata-Version: 2.2"));
}

#[tokio::test]
async fn test_upload_sdist_missing_pkg_info_is_bad_request() {
    let h = harness().await;
    let sdist = fixture_sdist_without_pkg_info();
    let fields = vec![
        (":action", "file_upload"),
        ("name", "velodexpkg"),
        ("version", "1.0"),
        ("filetype", "sdist"),
    ];
    let (content_type, body) = multipart_body(&fields, Some(("velodexpkg-1.0.tar.gz", &sdist)));
    let (status, body) = post_upload_response(&h.state, "/local/", Some(&upload_auth()), &content_type, body).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("uploaded content does not match the filename format: invalid sdist: missing required"));
}

#[tokio::test]
async fn test_upload_same_file_is_idempotent() {
    let h = harness().await;
    let wheel = fixture_wheel();
    assert_eq!(upload_velodexpkg(&h.state, "/local/", &wheel).await, StatusCode::OK);
    assert_eq!(upload_velodexpkg(&h.state, "/local/", &wheel).await, StatusCode::OK);

    let (status, _, detail) = get(&h.state, "/local/simple/velodexpkg/", Some("application/json")).await;
    let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["files"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn test_upload_same_filename_with_different_bytes_is_bad_request() {
    let h = harness().await;
    assert_eq!(
        upload_velodexpkg(&h.state, "/local/", &fixture_wheel()).await,
        StatusCode::OK
    );
    let wheel = fixture_wheel_with_body("1.0", b"VALUE = 2\n");
    let (ct, body) = multipart_body(&upload_fields(), Some(("velodexpkg-1.0-py3-none-any.whl", &wheel)));

    let (status, body) = post_upload_response(&h.state, "/local/", Some(&upload_auth()), &ct, body).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body,
        "File already exists: \"velodexpkg-1.0-py3-none-any.whl\" has different content; use a different filename"
    );
}

#[tokio::test]
async fn test_upload_duplicate_content_field_is_bad_request() {
    let h = harness().await;
    let wheel = fixture_wheel();
    let (content_type, body) = multipart_body_with_content_parts(
        &upload_fields(),
        &[
            ("velodexpkg-1.0-py3-none-any.whl", &wheel),
            ("velodexpkg-1.0-py3-none-any.whl", &wheel),
        ],
    );
    let (status, body) = post_upload_response(&h.state, "/local/", Some(&upload_auth()), &content_type, body).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, "bad upload: duplicate content field");
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
async fn test_upload_invalid_filename_is_bad_request() {
    let h = harness().await;
    let (ct, body) = multipart_body(&upload_fields(), Some(("velodexpkg/1.0.whl", b"x")));
    let (status, body) = post_upload_response(&h.state, "/root/pypi/", Some(&upload_auth()), &ct, body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("invalid filename"));
    assert!(body.contains("without separators"));
}

#[tokio::test]
async fn test_upload_invalid_distribution_filename_is_bad_request() {
    for (filename, expected) in [
        (
            "velodexpkg-1.0.zip",
            "invalid distribution filename \"velodexpkg-1.0.zip\": accepted upload formats are .whl and .tar.gz",
        ),
        (
            "velodexpkg-1.0.egg",
            "invalid distribution filename \"velodexpkg-1.0.egg\": legacy .egg uploads are not accepted; upload a wheel or .tar.gz sdist",
        ),
        (
            "velodexpkg-1.0-py3-none.whl",
            "invalid distribution filename \"velodexpkg-1.0-py3-none.whl\": wheel filenames must use distribution-version(-build tag)?-python tag-abi tag-platform tag.whl",
        ),
        (
            "velodexpkg.tar.gz",
            "invalid distribution filename \"velodexpkg.tar.gz\": sdist filenames must use name-version.tar.gz",
        ),
        (
            "velodexpkg!-1.0-py3-none-any.whl",
            "invalid distribution filename \"velodexpkg!-1.0-py3-none-any.whl\": distribution name component \"velodexpkg!\" is not a valid PyPA project name",
        ),
        (
            "velodexpkg-bad-py3-none-any.whl",
            "invalid distribution filename \"velodexpkg-bad-py3-none-any.whl\": version component \"bad\" is not a PEP 440 version",
        ),
        (
            "velodexpkg-1.0-py3-*-any.whl",
            "invalid distribution filename \"velodexpkg-1.0-py3-*-any.whl\": wheel build/tag component \"*\" contains invalid characters",
        ),
    ] {
        let h = harness().await;
        let (ct, body) = multipart_body(&upload_fields(), Some((filename, b"x")));
        let (status, body) = post_upload_response(&h.state, "/root/pypi/", Some(&upload_auth()), &ct, body).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body, expected);
    }
}

#[tokio::test]
async fn test_upload_form_validation_errors_include_actionable_body() {
    let h = harness().await;
    for (fields, content, expected_status, expected_body) in [
        (
            vec![(":action", "submit"), ("name", "velodexpkg"), ("version", "1.0")],
            Some(("velodexpkg-1.0-py3-none-any.whl", b"x".as_slice())),
            StatusCode::BAD_REQUEST,
            "unsupported :action",
        ),
        (
            vec![(":action", "file_upload"), ("version", "1.0")],
            Some(("velodexpkg-1.0-py3-none-any.whl", b"x".as_slice())),
            StatusCode::BAD_REQUEST,
            "missing required field: name",
        ),
        (
            vec![
                (":action", "file_upload"),
                ("name", "velodexpkg"),
                ("version", "1.0"),
                ("filetype", "bdist_wheel"),
            ],
            None,
            StatusCode::BAD_REQUEST,
            "missing required field: content",
        ),
        (
            vec![(":action", "file_upload"), ("name", "-bad"), ("version", "1.0")],
            Some(("velodexpkg-1.0-py3-none-any.whl", b"x".as_slice())),
            StatusCode::BAD_REQUEST,
            "invalid project name \"-bad\": names must start and end with an ASCII letter or digit and contain only letters, digits, '.', '_' or '-'",
        ),
        (
            vec![(":action", "file_upload"), ("name", "velodexpkg"), ("version", "bad")],
            Some(("velodexpkg-1.0-py3-none-any.whl", b"x".as_slice())),
            StatusCode::BAD_REQUEST,
            "invalid version \"bad\": expected a PEP 440 version",
        ),
        (
            vec![
                (":action", "file_upload"),
                ("name", "velodexpkg"),
                ("version", "1.0"),
                ("filetype", "sdist"),
            ],
            Some(("velodexpkg-1.0-py3-none-any.whl", b"x".as_slice())),
            StatusCode::BAD_REQUEST,
            "filetype \"sdist\" does not match filename; expected \"bdist_wheel\"",
        ),
        (
            upload_fields(),
            Some(("other-1.0-py3-none-any.whl", b"x".as_slice())),
            StatusCode::BAD_REQUEST,
            "filename project \"other\" does not match upload name \"velodexpkg\"",
        ),
        (
            upload_fields(),
            Some(("velodexpkg-2.0-py3-none-any.whl", b"x".as_slice())),
            StatusCode::BAD_REQUEST,
            "filename version \"2.0\" does not match upload version \"1.0\"",
        ),
        (
            vec![
                (":action", "file_upload"),
                ("name", "velodexpkg"),
                ("version", "1.0"),
                ("filetype", "bdist_wheel"),
                ("sha256_digest", "00"),
            ],
            Some(("velodexpkg-1.0-py3-none-any.whl", b"x".as_slice())),
            StatusCode::BAD_REQUEST,
            "sha256_digest value \"00\" is not lowercase hex with the expected length",
        ),
        (
            vec![
                (":action", "file_upload"),
                ("name", "velodexpkg"),
                ("version", "1.0"),
                ("filetype", "bdist_wheel"),
                ("md5_digest", "d41d8cd98f00b204e9800998ecf8427e"),
            ],
            Some(("velodexpkg-1.0-py3-none-any.whl", b"x".as_slice())),
            StatusCode::BAD_REQUEST,
            "md5_digest is not accepted without a sha256_digest or blake2_256_digest",
        ),
    ] {
        assert_upload_response(&h, &fields, content, expected_status, expected_body).await;
    }
}

#[tokio::test]
async fn test_upload_digest_and_requires_python_errors_include_actionable_body() {
    let h = harness().await;
    let wrong = "00".repeat(32);
    let fields = vec![
        (":action", "file_upload"),
        ("name", "velodexpkg"),
        ("version", "1.0"),
        ("filetype", "bdist_wheel"),
        ("sha256_digest", wrong.as_str()),
    ];
    assert_upload_response(
        &h,
        &fields,
        Some(("velodexpkg-1.0-py3-none-any.whl", b"x")),
        StatusCode::BAD_REQUEST,
        "sha256_digest mismatch",
    )
    .await;

    let fields = vec![
        (":action", "file_upload"),
        ("name", "velodexpkg"),
        ("version", "1.0"),
        ("filetype", "bdist_wheel"),
        ("requires_python", "=>3"),
    ];
    let wheel = fixture_wheel();
    assert_upload_response(
        &h,
        &fields,
        Some(("velodexpkg-1.0-py3-none-any.whl", &wheel)),
        StatusCode::BAD_REQUEST,
        "invalid Requires-Python value \"=>3\": expected PEP 440 version specifiers",
    )
    .await;
}

#[tokio::test]
async fn test_upload_content_validation_errors_include_actionable_body() {
    let h = harness().await;
    assert_upload_response(
        &h,
        &upload_fields(),
        Some(("velodexpkg-1.0-py3-none-any.whl", b"not a zip")),
        StatusCode::BAD_REQUEST,
        "uploaded content does not match the filename format: archive read failed: invalid Zip archive: Could not find EOCD",
    )
    .await;

    assert_upload_response(
        &h,
        &upload_fields(),
        Some((
            "velodexpkg-1.0-py3-none-any.whl",
            fixture_wheel_without_metadata().as_slice(),
        )),
        StatusCode::BAD_REQUEST,
        "uploaded content does not match the filename format: invalid wheel: missing required velodexpkg-1.0.dist-info/METADATA",
    )
    .await;

    assert_upload_response(
        &h,
        &upload_fields(),
        Some((
            "velodexpkg-1.0-py3-none-any.whl",
            fixture_wheel_with_metadata(b"\xff").as_slice(),
        )),
        StatusCode::BAD_REQUEST,
        "artifact metadata is not valid UTF-8",
    )
    .await;

    assert_upload_response(
        &h,
        &upload_fields(),
        Some((
            "velodexpkg-1.0-py3-none-any.whl",
            fixture_wheel_with_metadata(b"Metadata-Version: 2.1\nName: other\nVersion: 1.0\n").as_slice(),
        )),
        StatusCode::BAD_REQUEST,
        "metadata Name \"other\" does not match upload name \"velodexpkg\"",
    )
    .await;

    assert_upload_response(
        &h,
        &upload_fields(),
        Some((
            "velodexpkg-1.0-py3-none-any.whl",
            fixture_wheel_with_metadata(b"Metadata-Version: 2.1\nName: velodexpkg\nVersion: 2.0\n").as_slice(),
        )),
        StatusCode::BAD_REQUEST,
        "metadata Version \"2.0\" does not match upload version \"1.0\"",
    )
    .await;

    h.clock.store(i64::MAX, Ordering::Relaxed);
    let wheel = fixture_wheel();
    assert_upload_response(
        &h,
        &upload_fields(),
        Some(("velodexpkg-1.0-py3-none-any.whl", &wheel)),
        StatusCode::INTERNAL_SERVER_ERROR,
        "configured clock produced an invalid upload timestamp",
    )
    .await;
}

#[tokio::test]
async fn test_upload_metadata_form_fields_are_validated() {
    let h = harness().await;
    let fields = vec![
        (":action", "file_upload"),
        ("metadata_version", "2.1"),
        ("name", "velodexpkg"),
        ("version", "1.0"),
        ("requires_python", ">=3.8"),
        ("license", "MIT"),
        ("license_expression", "MIT"),
        ("license_file", "LICENSE"),
        ("provides_extra", "cli"),
        ("project_urls", "Source, https://example.test/source"),
        ("home_page", "https://example.test/home"),
        ("filetype", "bdist_wheel"),
    ];
    let wheel = fixture_wheel_with_metadata(
        b"Metadata-Version: 2.1\nName: velodexpkg\nVersion: 1.0\nRequires-Python: >=3.8\nLicense: MIT\nLicense-Expression: MIT\nLicense-File: LICENSE\nProvides-Extra: cli\nProject-URL: Source, https://example.test/source\nHome-Page: https://example.test/home\n",
    );
    let (content_type, body) = multipart_body(&fields, Some(("velodexpkg-1.0-py3-none-any.whl", &wheel)));
    assert_eq!(
        post_upload(&h.state, "/local/", Some(&upload_auth()), &content_type, body).await,
        StatusCode::OK
    );

    let mut fields = fields;
    fields[6] = ("license_expression", "Apache-2.0");
    assert_upload_response(
        &h,
        &fields,
        Some(("velodexpkg-1.0-py3-none-any.whl", &wheel)),
        StatusCode::BAD_REQUEST,
        "metadata License-Expression \"MIT\" does not match upload value \"Apache-2.0\"",
    )
    .await;
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
async fn test_upload_large_text_field_is_bad_request() {
    let h = harness().await;
    let large_name = "x".repeat(64 * 1024 + 1);
    let fields = vec![(":action", "file_upload"), ("name", large_name.as_str())];
    let (ct, body) = multipart_body(&fields, Some(("x-1.0.whl", b"x")));
    let (status, body) = post_upload_response(&h.state, "/root/pypi/", Some(&upload_auth()), &ct, body).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("upload field \"name\" exceeds 65536 bytes"));
}

#[tokio::test]
async fn test_upload_malformed_multipart_is_bad_request() {
    let h = harness().await;
    let body = b"--b\r\nContent-Disposition: form-data; name=\"name\"\r\n".to_vec();
    let (status, body) = post_upload_response(
        &h.state,
        "/root/pypi/",
        Some(&upload_auth()),
        "multipart/form-data; boundary=b",
        body,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.starts_with("bad upload: "));
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
    let wheel = fixture_wheel();
    let digest = Digest::of(&wheel);
    let fields = vec![
        (":action", "file_upload"),
        ("name", "velodexpkg"),
        ("version", "1.0"),
        ("filetype", "bdist_wheel"),
        ("sha256_digest", digest.as_str()),
        ("blake2_256_digest", ""),
        ("summary", "ignored"),
    ];
    let (ct, body) = multipart_body(&fields, Some(("velodexpkg-1.0-py3-none-any.whl", &wheel)));
    assert_eq!(
        post_upload(&h.state, "/root/pypi/", Some(&upload_auth()), &ct, body).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn test_upload_rejects_archived_and_quarantined_projects() {
    for status in ["archived", "quarantined"] {
        let h = harness().await;
        let digest = Digest::of(b"upstream");
        let file_url = format!("{}/files/velodexpkg.whl", h.server.uri());
        mount_status_detail(&h.server, "velodexpkg", status, "policy", digest.as_str(), &file_url).await;

        let (content_type, body) = multipart_body(
            &upload_fields(),
            Some(("velodexpkg-1.0-py3-none-any.whl", &fixture_wheel())),
        );
        let (code, body) =
            post_upload_response(&h.state, "/root/pypi/", Some(&upload_auth()), &content_type, body).await;

        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(
            body,
            format!("project \"velodexpkg\" is {status}; uploads are disabled")
        );
    }
}

#[tokio::test]
async fn test_upload_allows_deprecated_project() {
    let h = harness().await;
    let digest = Digest::of(b"upstream");
    let file_url = format!("{}/files/velodexpkg.whl", h.server.uri());
    mount_status_detail(
        &h.server,
        "velodexpkg",
        "deprecated",
        "use another package",
        digest.as_str(),
        &file_url,
    )
    .await;

    assert_eq!(
        upload_velodexpkg(&h.state, "/root/pypi/", &fixture_wheel()).await,
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
async fn test_upload_corrupt_existing_record_is_server_error() {
    let h = harness().await;
    h.state
        .meta
        .put_upload("local", "velodexpkg", "velodexpkg-1.0-py3-none-any.whl", b"not-json")
        .unwrap();
    let wheel = fixture_wheel();
    let (content_type, body) = multipart_body(&upload_fields(), Some(("velodexpkg-1.0-py3-none-any.whl", &wheel)));

    let (status, body) = post_upload_response(&h.state, "/local/", Some(&upload_auth()), &content_type, body).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body.contains("upload storage on index \"local\" for project \"velodexpkg\""));
    assert!(body.contains("simple API document could not be parsed"));
}

#[tokio::test]
async fn test_yank_and_unyank_and_delete() {
    let h = harness().await;
    upload_velodexpkg(&h.state, "/root/pypi/", &fixture_wheel()).await;

    // Yank the version, then the file is served with the recorded reason.
    assert_eq!(
        request(
            &h.state,
            "PUT",
            "/root/pypi/velodexpkg/1.0/yank?ignored=1&reason=bad+build",
            Some(&upload_auth())
        )
        .await,
        StatusCode::OK
    );
    let (_, _, yanked) = get(&h.state, "/root/pypi/simple/velodexpkg/", Some("application/json")).await;
    assert!(yanked.contains("\"yanked\":\"bad build\""));

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
    upload_velodexpkg(&h.state, "/local/", &fixture_wheel()).await;
    assert_eq!(
        request(&h.state, "DELETE", "/local/velodexpkg/1.0/", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (status, ..) = get(&h.state, "/local/simple/velodexpkg/", Some("application/json")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_admin_routes_decode_safe_project_and_version_segments() {
    let h = harness().await;
    upload_version(&h.state, "/local/", "1.0+local", b"PKlocal").await;
    assert_eq!(
        request(
            &h.state,
            "DELETE",
            "/local/velodexpkg/1.0%2Blocal/",
            Some(&upload_auth())
        )
        .await,
        StatusCode::OK
    );
    let (status, ..) = get(&h.state, "/local/simple/velodexpkg/", Some("application/json")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_admin_routes_reject_decoded_separators() {
    let h = harness().await;
    assert_eq!(
        request(&h.state, "DELETE", "/local/velo%2Fdexpkg/", Some(&upload_auth())).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(&h.state, "DELETE", "/local/velodexpkg/1.0%2Fbad/", Some(&upload_auth())).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(&h.state, "DELETE", "/local/velo%xxdexpkg/", Some(&upload_auth())).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(&h.state, "DELETE", "/local/velodexpkg/1.0%xxbad/", Some(&upload_auth())).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(&h.state, "PUT", "/local/velo%2Fdexpkg/yank", Some(&upload_auth())).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(&h.state, "PUT", "/local/velo%2Fdexpkg/restore", Some(&upload_auth())).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(&h.state, "DELETE", "/local/velo%2Fdexpkg/yank", Some(&upload_auth())).await,
        StatusCode::BAD_REQUEST
    );
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
    upload_velodexpkg(&h.state, "/local/", &fixture_wheel()).await;
    assert_eq!(
        request(&h.state, "DELETE", "/local/velodexpkg/", None).await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn test_delete_on_non_volatile_is_forbidden() {
    let h = harness_with(true, false).await;
    upload_velodexpkg(&h.state, "/local/", &fixture_wheel()).await;
    let (status, body) = request_response(&h.state, "DELETE", "/local/velodexpkg/", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body, "file removal: index is not volatile; delete is disabled");
}

#[tokio::test(flavor = "current_thread")]
async fn test_security_logs_delete_policy_denial() {
    let h = harness_with(true, false).await;
    upload_velodexpkg(&h.state, "/local/", &fixture_wheel()).await;
    let logs = LogCapture::default();
    let guard = logs.install();

    assert_eq!(
        request(&h.state, "DELETE", "/local/velodexpkg/", Some(&upload_auth())).await,
        StatusCode::FORBIDDEN
    );

    drop(guard);
    let events = logs.security_events();
    let delete = events
        .iter()
        .find(|event| field(event, "action") == Some("delete") && field(event, "result") == Some("denied"))
        .unwrap();
    assert_eq!(field(delete, "actor"), Some("__token__"));
    assert_eq!(field(delete, "repository"), Some("local"));
    assert_eq!(field(delete, "local_repository"), Some("local"));
    assert_eq!(field(delete, "project"), Some("velodexpkg"));
    assert_eq!(
        field(delete, "reason"),
        Some("index is not volatile; delete is disabled")
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
async fn test_put_suffix_inside_segment_is_not_an_action() {
    let h = harness().await;
    assert_eq!(
        request(&h.state, "PUT", "/local/velodexpkg/1.0/notyank", Some(&upload_auth())).await,
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
    assert_eq!(
        upload_velodexpkg(&state, "/a/b/", &fixture_wheel()).await,
        StatusCode::OK
    );
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
    assert!(body.contains(&h.server.uri()));
    assert!(!body.contains("\"project_count\""));
    assert!(!body.contains("\"upload_count\""));
    assert!(!body.contains("\"recent_uploads\""));
    assert!(!body.contains("s3cret"));
}

#[tokio::test]
async fn test_status_admin_details_include_bounded_summaries() {
    let h = harness().await;
    assert_eq!(
        upload_velodexpkg(&h.state, "/root/pypi/", &fixture_wheel()).await,
        StatusCode::OK
    );
    let (status, _, body) = get(&h.state, "/+status?details=admin", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"project_count\""));
    assert!(body.contains("\"upload_count\""));
    assert!(body.contains("\"recent_uploads\""));
    assert!(body.contains("velodexpkg-1.0-py3-none-any.whl"));
}

#[tokio::test]
async fn test_status_redacts_upstream_and_upload_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let indexes = vec![
        Index {
            name: "private".to_owned(),
            route: "private".to_owned(),
            kind: IndexKind::Mirror(
                UpstreamClient::with_auth(
                    "https://user:pass@example.invalid/simple/?token=url-secret#frag",
                    Auth::Bearer("bearer-secret".to_owned()),
                )
                .unwrap(),
            ),
        },
        Index {
            name: "local".to_owned(),
            route: "local".to_owned(),
            kind: IndexKind::Local {
                upload_token: Some("upload-secret".to_owned()),
                volatile: false,
            },
        },
    ];
    let state = Arc::new(AppState::new(meta, blobs, 60, indexes));
    let (status, _, body) = get(&state, "/+status", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("https://example.invalid/simple/"));
    assert!(body.contains("\"kind\":\"bearer\""));
    assert!(body.contains("<redacted>"));
    for secret in ["user", "pass", "url-secret", "bearer-secret", "upload-secret"] {
        assert!(!body.contains(secret));
    }
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

#[tokio::test]
async fn test_index_response_error_is_bad_gateway() {
    use crate::cache::CacheError;
    use crate::handlers::{Format, index_response};
    let response = index_response(Err(CacheError::Unavailable), Format::Json, "pypi");
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        String::from_utf8_lossy(&body),
        "project list on index \"pypi\": upstream is unavailable and no cached page exists"
    );
}

async fn upload_version(state: &Arc<AppState>, uri: &str, version: &str, _wheel: &[u8]) -> StatusCode {
    let wheel = fixture_wheel_for(version);
    let fields = vec![
        (":action", "file_upload"),
        ("name", "velodexpkg"),
        ("version", version),
        ("filetype", "bdist_wheel"),
    ];
    let filename = format!("velodexpkg-{version}-py3-none-any.whl");
    let (ct, body) = multipart_body(&fields, Some((&filename, &wheel)));
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
    upload_velodexpkg(&h.state, "/local/", &fixture_wheel()).await;
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
async fn test_metadata_digest_mismatch_is_server_error() {
    let h = harness().await;
    let artifact = Digest::of(b"artifact");
    let metadata = Digest::of(b"expected");
    let metadata_url = format!("{}/files/pkg.whl.metadata", h.server.uri());
    h.state
        .meta
        .put_metadata(artifact.as_str(), &metadata_url, metadata.as_str(), "pypi")
        .unwrap();
    Mock::given(method("GET"))
        .and(path("/files/pkg.whl.metadata"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"wrong".to_vec()))
        .mount(&h.server)
        .await;

    let uri = format!("/pypi/files/{}/pkg.whl.metadata", artifact.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body.contains("metadata fetch on index \"pypi\" for file \"pkg.whl.metadata\""));
    assert!(body.contains("blob store error: digest mismatch"));
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

    let status = request(
        &h.state,
        "PUT",
        "/root/pypi/flask/1.0/yank?reason=bad+build",
        Some(&upload_auth()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The overlay page carries the marker; the mirror's own route stays untouched.
    let (_, _, merged) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;
    assert!(merged.contains("\"yanked\":\"bad build\""));
    let (_, _, mirror) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    assert!(!mirror.contains("\"yanked\":\"bad build\""));

    // Un-yank clears the override.
    let status = request(&h.state, "DELETE", "/root/pypi/flask/1.0/yank", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, cleared) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;
    assert!(!cleared.contains("\"yanked\":true"));

    let status = request(&h.state, "PUT", "/root/pypi/flask/1.0/yank", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, yanked) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;
    assert!(yanked.contains("\"yanked\":true"));
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
async fn test_promote_requires_source_query() {
    let h = harness().await;
    let (status, body) = request_response(&h.state, "PUT", "/root/pypi/flask/1.0/promote", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, "promotion requires from={source route}");

    let (status, body) = request_response(
        &h.state,
        "PUT",
        "/root/pypi/flask/1.0/promote?source=local",
        Some(&upload_auth()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, "promotion requires from={source route}");
}

#[tokio::test]
async fn test_promote_requires_version() {
    let h = promotion_harness().await;
    let (status, body) = request_response(
        &h.state,
        "PUT",
        "/prod/velodexpkg/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, "promotion requires a version");
}

#[tokio::test]
async fn test_promote_rejects_invalid_project_path() {
    let h = promotion_harness().await;
    let (status, body) = request_response(
        &h.state,
        "PUT",
        "/prod/velodexpkg%2Fbad/1.0/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body,
        "invalid project \"velodexpkg/bad\": path parameters must be non-empty segments without separators, \
         traversal, or control characters"
    );
}

#[tokio::test]
async fn test_promote_copies_release_records_without_copying_blobs() {
    let h = promotion_harness().await;
    let wheel = fixture_wheel();
    let digest = upload_wheel_to(&h.state, "/staging/", "velodexpkg-1.0-py3-none-any.whl", "1.0", &wheel).await;
    upload_wheel_to(
        &h.state,
        "/staging/",
        "velodexpkg-2.0-py3-none-any.whl",
        "2.0",
        &fixture_wheel_with_body("2.0", b"VALUE = 2\n"),
    )
    .await;
    assert_eq!(
        request(
            &h.state,
            "PUT",
            "/staging/velodexpkg/1.0/yank?reason=bad+build",
            Some(&upload_auth()),
        )
        .await,
        StatusCode::OK
    );
    let blobs_before = blob_count(&h.state);

    let (status, body) = request_response(
        &h.state,
        "PUT",
        "/prod/velodexpkg/1.0/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "promoted 1 file(s)");
    assert_eq!(blob_count(&h.state), blobs_before);
    let (_, _, body) = get(&h.state, "/prod/simple/velodexpkg/", Some("application/json")).await;
    let detail: serde_json::Value = serde_json::from_str(&body).unwrap();
    let file = &detail["files"][0];
    assert_eq!(
        file,
        &serde_json::json!({
            "filename": "velodexpkg-1.0-py3-none-any.whl",
            "url": format!("/prod/files/{}/velodexpkg-1.0-py3-none-any.whl", digest.as_str()),
            "hashes": {"sha256": digest.as_str()},
            "requires-python": ">=3.8",
            "size": wheel.len() as u64,
            "upload-time": "1970-01-01T00:16:40Z",
            "yanked": "bad build",
            "core-metadata": file["core-metadata"].clone(),
            "dist-info-metadata": file["core-metadata"].clone()
        })
    );
    assert!(
        file["core-metadata"]["sha256"]
            .as_str()
            .is_some_and(|sha256| sha256.len() == 64)
    );
    let metadata_uri = format!(
        "/prod/files/{}/velodexpkg-1.0-py3-none-any.whl.metadata",
        digest.as_str()
    );
    let (metadata_status, _, metadata) = get(&h.state, &metadata_uri, None).await;
    assert_eq!(metadata_status, StatusCode::OK);
    assert!(metadata.contains("Name: velodexpkg"));
    assert_eq!(detail["files"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn test_promote_skips_target_file_with_same_digest() {
    let h = promotion_harness().await;
    let wheel = fixture_wheel();
    upload_wheel_to(&h.state, "/staging/", "velodexpkg-1.0-py3-none-any.whl", "1.0", &wheel).await;
    upload_wheel_to(&h.state, "/prod/", "velodexpkg-1.0-py3-none-any.whl", "1.0", &wheel).await;
    let logs = LogCapture::default();
    let _guard = logs.install();

    let (status, body) = request_response(
        &h.state,
        "PUT",
        "/prod/velodexpkg/1.0/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;

    let event = logs
        .security_events()
        .into_iter()
        .find(|event| field(event, "action") == Some("promote"))
        .unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "promoted 0 file(s)");
    assert_eq!(field(&event, "action"), Some("promote"));
    assert_eq!(field(&event, "result"), Some("noop"));
    assert_eq!(field(&event, "repository"), Some("prod"));
    assert_eq!(field(&event, "source_repository"), Some("staging"));
    assert_eq!(field(&event, "reason"), Some("same files already exist on target"));
}

#[tokio::test]
async fn test_promote_reports_missing_sha256_in_source_record() {
    let h = promotion_harness().await;
    let filename = "velodexpkg-1.0-py3-none-any.whl";
    let uploaded = upload_record(
        filename,
        "1.0",
        "https://example.test/pkg.whl".to_owned(),
        BTreeMap::new(),
        Some(4),
    );
    h.state
        .meta
        .put_upload("staging", "velodexpkg", filename, &to_json(&uploaded).into_bytes())
        .unwrap();
    h.state.meta.put_project("staging", "velodexpkg", "velodexpkg").unwrap();
    let logs = LogCapture::default();
    let _guard = logs.install();

    let (status, body) = request_response(
        &h.state,
        "PUT",
        "/prod/velodexpkg/1.0/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;

    let event = logs
        .security_events()
        .into_iter()
        .find(|event| field(event, "action") == Some("promote"))
        .unwrap();
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        body,
        "promotion: uploaded file \"velodexpkg-1.0-py3-none-any.whl\" has no sha256 hash"
    );
    assert_eq!(field(&event, "result"), Some("failure"));
    assert_eq!(
        field(&event, "reason"),
        Some("uploaded file \"velodexpkg-1.0-py3-none-any.whl\" has no sha256 hash")
    );
}

#[tokio::test]
async fn test_promote_uses_normalized_name_when_source_display_is_missing() {
    let h = promotion_harness().await;
    let filename = "velodexpkg-1.0-py3-none-any.whl";
    let digest = Digest::of(b"wheel");
    let uploaded = upload_record(
        filename,
        "1.0",
        local_file_url("staging", digest.as_str(), filename),
        BTreeMap::from([("sha256".to_owned(), digest.as_str().to_owned())]),
        Some(5),
    );
    h.state
        .meta
        .put_upload("staging", "velodexpkg", filename, &to_json(&uploaded).into_bytes())
        .unwrap();

    let (status, body) = request_response(
        &h.state,
        "PUT",
        "/prod/velodexpkg/1.0/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;

    let (_, _, detail) = get(&h.state, "/prod/simple/velodexpkg/", Some("application/json")).await;
    let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "promoted 1 file(s)");
    assert_eq!(detail["name"], "velodexpkg");
}

#[tokio::test]
async fn test_promote_reports_no_matching_source_release() {
    let h = promotion_harness().await;

    let (status, body) = request_response(
        &h.state,
        "PUT",
        "/prod/velodexpkg/1.0/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body,
        "promotion: no uploaded files on source \"staging\" match project \"velodexpkg\" version \"1.0\""
    );
}

#[tokio::test]
async fn test_promote_conflicts_on_target_filename_with_different_bytes() {
    let h = promotion_harness().await;
    upload_wheel_to(
        &h.state,
        "/staging/",
        "velodexpkg-1.0-py3-none-any.whl",
        "1.0",
        &fixture_wheel(),
    )
    .await;
    upload_wheel_to(
        &h.state,
        "/prod/",
        "velodexpkg-1.0-py3-none-any.whl",
        "1.0",
        &fixture_wheel_with_body("1.0", b"VALUE = 2\n"),
    )
    .await;

    let (status, body) = request_response(
        &h.state,
        "PUT",
        "/prod/velodexpkg/1.0/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body,
        "File already exists: \"velodexpkg-1.0-py3-none-any.whl\" has different content; use a different filename"
    );
}

#[tokio::test]
async fn test_promote_rejects_invalid_source_and_target_routes() {
    let h = promotion_harness().await;
    upload_wheel_to(
        &h.state,
        "/staging/",
        "velodexpkg-1.0-py3-none-any.whl",
        "1.0",
        &fixture_wheel(),
    )
    .await;

    assert_eq!(
        request(
            &h.state,
            "PUT",
            "/missing/velodexpkg/1.0/promote?from=staging",
            Some(&upload_auth()),
        )
        .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            &h.state,
            "PUT",
            "/prod/velodexpkg/1.0/promote?from=missing",
            Some(&upload_auth()),
        )
        .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            &h.state,
            "PUT",
            "/prod/velodexpkg/1.0/promote?from=pypi",
            Some(&upload_auth()),
        )
        .await,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        request(
            &h.state,
            "PUT",
            "/pypi/velodexpkg/1.0/promote?from=staging",
            Some(&upload_auth()),
        )
        .await,
        StatusCode::METHOD_NOT_ALLOWED
    );
}

#[tokio::test]
async fn test_promote_rejects_archived_and_quarantined_targets() {
    for status in ["archived", "quarantined"] {
        let h = promotion_harness().await;
        upload_wheel_to(
            &h.state,
            "/staging/",
            "velodexpkg-1.0-py3-none-any.whl",
            "1.0",
            &fixture_wheel(),
        )
        .await;
        let digest = Digest::of(b"upstream");
        let file_url = format!("{}/files/velodexpkg.whl", h.server.uri());
        mount_status_detail(&h.server, "velodexpkg", status, "policy", digest.as_str(), &file_url).await;

        let (code, body) = request_response(
            &h.state,
            "PUT",
            "/release/velodexpkg/1.0/promote?from=staging",
            Some(&upload_auth()),
        )
        .await;

        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(
            body,
            format!("project \"velodexpkg\" is {status}; uploads are disabled")
        );
        assert_eq!(
            get(&h.state, "/prod/simple/velodexpkg/", Some("application/json"))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }
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

pub(super) fn fixture_wheel() -> Vec<u8> {
    fixture_wheel_for("1.0")
}

pub(super) fn fixture_sdist() -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let encoder = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);
        let content = b"Metadata-Version: 2.2\nName: velodexpkg\nVersion: 1.0\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "velodexpkg-1.0/PKG-INFO", content.as_slice())
            .unwrap();
        let pyproject = b"[build-system]\nrequires = []\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(pyproject.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "velodexpkg-1.0/pyproject.toml", pyproject.as_slice())
            .unwrap();
        tar.finish().unwrap();
    }
    buf
}

fn fixture_sdist_without_pkg_info() -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let encoder = flate2::write::GzEncoder::new(&mut buf, flate2::Compression::default());
        let mut tar = tar::Builder::new(encoder);
        let content = b"x = 1\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "velodexpkg-1.0/module.py", content.as_slice())
            .unwrap();
        let pyproject = b"[build-system]\nrequires = []\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(pyproject.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, "velodexpkg-1.0/pyproject.toml", pyproject.as_slice())
            .unwrap();
        tar.finish().unwrap();
    }
    buf
}

fn fixture_wheel_for(version: &str) -> Vec<u8> {
    fixture_wheel_with_body(version, b"VALUE = 1\n")
}

fn fixture_wheel_with_body(version: &str, body: &[u8]) -> Vec<u8> {
    fixture_wheel_with_body_and_metadata(
        version,
        body,
        Some(
            format!("Metadata-Version: 2.1\nName: velodexpkg\nVersion: {version}\nRequires-Python: >=3.8\n").as_bytes(),
        ),
    )
}

fn fixture_wheel_without_metadata() -> Vec<u8> {
    fixture_wheel_with_body_and_metadata("1.0", b"VALUE = 1\n", None)
}

fn fixture_wheel_with_metadata(metadata: &[u8]) -> Vec<u8> {
    fixture_wheel_with_body_and_metadata("1.0", b"VALUE = 1\n", Some(metadata))
}

fn empty_zip() -> Vec<u8> {
    let mut bytes = Vec::new();
    zip::ZipWriter::new(std::io::Cursor::new(&mut bytes)).finish().unwrap();
    bytes
}

fn fixture_wheel_with_metadata_compression(metadata: &[u8], compression: zip::CompressionMethod) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default().compression_method(compression);
        let dist_info = "velodexpkg-1.0.dist-info";
        let wheel = b"Wheel-Version: 1.0\nGenerator: velodex-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n";
        let entries = [
            ("velodexpkg/__init__.py".to_owned(), b"VALUE = 1\n".to_vec()),
            (format!("{dist_info}/METADATA"), metadata.to_vec()),
            (format!("{dist_info}/WHEEL"), wheel.to_vec()),
        ];
        for (path, bytes) in &entries {
            zip.start_file(path, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        let record_path = format!("{dist_info}/RECORD");
        zip.start_file(&record_path, options).unwrap();
        zip.write_all(record(&entries, &record_path).as_bytes()).unwrap();
        zip.finish().unwrap();
    }
    buf
}

fn wheel_with_invalid_deflated_metadata(metadata: &[u8]) -> Vec<u8> {
    let mut wheel = fixture_wheel_with_metadata(metadata);
    let data_start = metadata_local_data_start(&wheel);
    wheel[data_start] = 0x07;
    wheel
}

fn wheel_with_metadata_compression_method(metadata: &[u8], compression_method: u16) -> Vec<u8> {
    let mut wheel = fixture_wheel_with_metadata(metadata);
    let position = metadata_central_directory_position(&wheel);
    wheel[position + 10..position + 12].copy_from_slice(&compression_method.to_le_bytes());
    wheel
}

fn wheel_with_metadata_uncompressed_size(metadata: &[u8], uncompressed_size: u32) -> Vec<u8> {
    let mut wheel = fixture_wheel_with_metadata(metadata);
    let position = metadata_central_directory_position(&wheel);
    wheel[position + 24..position + 28].copy_from_slice(&uncompressed_size.to_le_bytes());
    wheel
}

fn overwrite_metadata_local_signature(wheel: &mut [u8], signature: [u8; 4]) {
    let position = metadata_local_header_position(wheel);
    wheel[position..position + 4].copy_from_slice(&signature);
}

fn overwrite_metadata_central_signature(wheel: &mut [u8], signature: [u8; 4]) {
    let position = metadata_central_directory_position(wheel);
    wheel[position..position + 4].copy_from_slice(&signature);
}

fn metadata_local_data_start(wheel: &[u8]) -> usize {
    let position = metadata_local_header_position(wheel);
    let name_len = usize::from(u16::from_le_bytes(
        wheel[position + 26..position + 28].try_into().unwrap(),
    ));
    let extra_len = usize::from(u16::from_le_bytes(
        wheel[position + 28..position + 30].try_into().unwrap(),
    ));
    position + 30 + name_len + extra_len
}

fn metadata_local_header_position(wheel: &[u8]) -> usize {
    let metadata = b"velodexpkg-1.0.dist-info/METADATA";
    for position in 0..wheel.len().saturating_sub(30) {
        if !wheel[position..].starts_with(b"PK\x03\x04") {
            continue;
        }
        let name_len = usize::from(u16::from_le_bytes(
            wheel[position + 26..position + 28].try_into().unwrap(),
        ));
        let name_start = position + 30;
        let name_end = name_start + name_len;
        if wheel.get(name_start..name_end) == Some(metadata.as_slice()) {
            return position;
        }
    }
    panic!("metadata local header not found");
}

fn metadata_central_directory_position(wheel: &[u8]) -> usize {
    let metadata = b"velodexpkg-1.0.dist-info/METADATA";
    for position in 0..wheel.len().saturating_sub(46) {
        if !wheel[position..].starts_with(b"PK\x01\x02") {
            continue;
        }
        let name_len = usize::from(u16::from_le_bytes(
            wheel[position + 28..position + 30].try_into().unwrap(),
        ));
        let name_start = position + 46;
        let name_end = name_start + name_len;
        if wheel.get(name_start..name_end) == Some(metadata.as_slice()) {
            return position;
        }
    }
    panic!("metadata central directory entry not found");
}

fn fixture_wheel_with_body_and_metadata(version: &str, body: &[u8], metadata: Option<&[u8]>) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        let dist_info = format!("velodexpkg-{version}.dist-info");
        let wheel = b"Wheel-Version: 1.0\nGenerator: velodex-test\nRoot-Is-Purelib: true\nTag: py3-none-any\n";
        let mut entries = vec![("velodexpkg/__init__.py".to_owned(), body.to_vec())];
        if let Some(metadata) = metadata {
            entries.push((format!("{dist_info}/METADATA"), metadata.to_vec()));
        }
        entries.push((format!("{dist_info}/WHEEL"), wheel.to_vec()));
        for (path, bytes) in &entries {
            zip.start_file(path, options).unwrap();
            zip.write_all(bytes).unwrap();
        }
        let record_path = format!("{dist_info}/RECORD");
        zip.start_file(&record_path, options).unwrap();
        zip.write_all(record(&entries, &record_path).as_bytes()).unwrap();
        zip.finish().unwrap();
    }
    buf
}

fn record(entries: &[(String, Vec<u8>)], record_path: &str) -> String {
    let mut record = String::new();
    for (path, bytes) in entries {
        let digest = URL_SAFE_NO_PAD.encode(Sha256::digest(bytes));
        writeln!(record, "{path},sha256={digest},{}", bytes.len()).unwrap();
    }
    writeln!(record, "{record_path},,").unwrap();
    record
}

async fn upload_wheel(state: &Arc<AppState>, filename: &str, bytes: &[u8]) -> Digest {
    upload_wheel_to(state, "/local/", filename, "1.0", bytes).await
}

async fn upload_wheel_to(state: &Arc<AppState>, uri: &str, filename: &str, version: &str, bytes: &[u8]) -> Digest {
    let fields = vec![
        (":action", "file_upload"),
        ("name", "velodexpkg"),
        ("version", version),
        ("filetype", "bdist_wheel"),
    ];
    let (ct, body) = multipart_body(&fields, Some((filename, bytes)));
    assert_eq!(
        post_upload(state, uri, Some(&upload_auth()), &ct, body).await,
        StatusCode::OK
    );
    Digest::of(bytes)
}

fn blob_count(state: &AppState) -> u64 {
    let mut count = 0;
    state
        .blobs
        .scan(|_entry| {
            count += 1;
            Ok::<(), std::io::Error>(())
        })
        .unwrap();
    count
}

fn upload_record(
    filename: &str,
    version: &str,
    url: String,
    hashes: BTreeMap<String, String>,
    size: Option<u64>,
) -> Uploaded {
    Uploaded {
        version: version.to_owned(),
        file: File {
            filename: filename.to_owned(),
            url,
            hashes,
            requires_python: None,
            size,
            upload_time: None,
            yanked: Yanked::No,
            core_metadata: CoreMetadata::Absent,
            dist_info_metadata: CoreMetadata::Absent,
            gpg_sig: None,
            provenance: Provenance::Absent,
        },
    }
}

fn put_local_file(state: &AppState, filename: &str, bytes: &[u8], version: &str) -> Digest {
    let digest = Digest::of(bytes);
    state.blobs.write_verified(bytes, &digest).unwrap();
    let uploaded = upload_record(
        filename,
        version,
        local_file_url("local", digest.as_str(), filename),
        BTreeMap::from([("sha256".to_owned(), digest.as_str().to_owned())]),
        Some(bytes.len() as u64),
    );
    state
        .meta
        .put_upload("local", "velodexpkg", filename, &to_json(&uploaded).into_bytes())
        .unwrap();
    state.meta.put_project("local", "velodexpkg", "velodexpkg").unwrap();
    digest
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
    assert_eq!(
        paths,
        [
            "velodexpkg-1.0.dist-info/METADATA",
            "velodexpkg-1.0.dist-info/RECORD",
            "velodexpkg-1.0.dist-info/WHEEL",
            "velodexpkg/__init__.py"
        ]
    );
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
async fn test_inspect_reads_query_member_content() {
    let h = harness().await;
    let digest = put_local_file(&h.state, "velodexpkg 1.0#x?.whl", &fixture_wheel(), "1.0");
    let uri = format!(
        "/local/inspect/{}/velodexpkg%201.0%23x%3F.whl?member=velodexpkg-1.0.dist-info%2FMETADATA",
        digest.as_str()
    );
    let (status, headers, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), "text/plain; charset=utf-8");
    assert!(body.starts_with("Metadata-Version: 2.1"));
}

#[tokio::test]
async fn test_inspect_query_without_member_lists_archive() {
    let h = harness().await;
    let digest = upload_wheel(&h.state, "velodexpkg-1.0-py3-none-any.whl", &fixture_wheel()).await;
    let uri = format!(
        "/local/inspect/{}/velodexpkg-1.0-py3-none-any.whl?ignored=1",
        digest.as_str()
    );
    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("velodexpkg-1.0.dist-info/METADATA"));
}

#[tokio::test]
async fn test_inspect_legacy_member_rejects_invalid_encoding() {
    let h = harness().await;
    let digest = upload_wheel(&h.state, "velodexpkg-1.0-py3-none-any.whl", &fixture_wheel()).await;
    let uri = format!("/local/inspect/{}/velodexpkg-1.0-py3-none-any.whl/%FF", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("invalid percent-encoded path segment"));
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
async fn test_inspect_rejects_bad_member_chunk_parameters() {
    let h = harness().await;
    let digest = upload_wheel(&h.state, "velodexpkg-1.0-py3-none-any.whl", &fixture_wheel()).await;
    let uri = format!(
        "/local/inspect/{}/velodexpkg-1.0-py3-none-any.whl?member=velodexpkg-1.0.dist-info%2FMETADATA",
        digest.as_str()
    );

    let (status, _, body) = get(&h.state, &format!("{uri}&limit=0"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("limit must be between 1 and"));

    let (status, _, body) = get(&h.state, &format!("{uri}&limit=nope"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("limit must be an integer between 1 and 1048576"));

    let (status, _, body) = get(&h.state, &format!("{uri}&offset=nope"), None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("offset must be a non-negative integer"));

    let (status, headers, body) = get(&h.state, &format!("{uri}&limit=8"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "Metadata");
    assert_eq!(headers.get("x-velodex-next-offset").unwrap(), "8");

    let (status, _, body) = get(&h.state, &format!("{uri}&offset=999999"), None).await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert!(body.contains("offset 999999 is beyond member size"));
}

#[tokio::test]
async fn test_inspect_unsupported_type() {
    let h = harness().await;
    let digest = put_local_file(&h.state, "velodexpkg-1.0.txt", b"not an archive", "1.0");
    let uri = format!("/local/inspect/{}/velodexpkg-1.0.txt", digest.as_str());
    let (status, ..) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn test_inspect_corrupt_archive_is_unprocessable() {
    let h = harness().await;
    let digest = put_local_file(&h.state, "velodexpkg-1.0-py3-none-any.whl", b"PK corrupt bytes", "1.0");
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
        let big = vec![b'a'; usize::try_from(crate::archive::DEFAULT_MEMBER_CHUNK + 1).unwrap()];
        let mut head = tar::Header::new_gnu();
        head.set_size(big.len() as u64);
        head.set_cksum();
        builder
            .append_data(&mut head, "velodexpkg-1.0/big.txt", big.as_slice())
            .unwrap();
        builder.into_inner().unwrap().finish().unwrap();
    }
    let digest = put_local_file(&h.state, "velodexpkg-1.0.tar.gz", &tarball, "1.0");

    let uri = format!("/local/inspect/{}/velodexpkg-1.0.tar.gz", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("setup.py"));

    let (status, _, content) = get(&h.state, &format!("{uri}/velodexpkg-1.0/setup.py"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content, "print()\n");

    let (status, headers, content) = get(&h.state, &format!("{uri}/velodexpkg-1.0/big.txt"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        content.len(),
        usize::try_from(crate::archive::DEFAULT_MEMBER_CHUNK).unwrap()
    );
    assert_eq!(
        headers.get("x-velodex-next-offset").unwrap(),
        crate::archive::DEFAULT_MEMBER_CHUNK.to_string().as_str()
    );

    let (status, headers, content) = get(
        &h.state,
        &format!(
            "{uri}/velodexpkg-1.0/big.txt?offset={}",
            crate::archive::DEFAULT_MEMBER_CHUNK
        ),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content.len(), 1);
    assert!(!headers.contains_key("x-velodex-next-offset"));
}

#[tokio::test]
async fn test_inspect_binary_member_rejected_for_inline_preview() {
    let h = harness().await;
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("data.bin", options).unwrap();
        zip.write_all(&[0xff, 0xfe, 0x00]).unwrap();
        zip.finish().unwrap();
    }
    let digest = put_local_file(&h.state, "velodexpkg-1.0-py3-none-any.whl", &buf, "1.0");
    let uri = format!(
        "/local/inspect/{}/velodexpkg-1.0-py3-none-any.whl/data.bin",
        digest.as_str()
    );
    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert!(body.contains("cannot be previewed inline"));
}

#[tokio::test]
async fn test_inspect_nested_archive_lists_selected_container_only() {
    let h = harness().await;
    let inner = {
        let mut buf = Vec::new();
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("pkg/mod.py", options).unwrap();
        zip.write_all(b"x = 1\n").unwrap();
        zip.finish().unwrap();
        buf
    };
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default();
        zip.start_file("vendor/inner.zip", options).unwrap();
        zip.write_all(&inner).unwrap();
        zip.finish().unwrap();
    }
    let digest = put_local_file(&h.state, "velodexpkg-1.0-py3-none-any.whl", &buf, "1.0");
    let uri = format!(
        "/local/inspect/{}/velodexpkg-1.0-py3-none-any.whl?container=vendor%2Finner.zip",
        digest.as_str()
    );

    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("pkg/mod.py"));

    let (status, _, content) = get(&h.state, &format!("{uri}&member=pkg%2Fmod.py"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content, "x = 1\n");
}

#[tokio::test]
async fn test_inspect_nested_archive_depth_limit_is_bad_request() {
    let h = harness().await;
    let digest = upload_wheel(&h.state, "velodexpkg-1.0-py3-none-any.whl", &fixture_wheel()).await;
    let mut uri = format!("/local/inspect/{}/velodexpkg-1.0-py3-none-any.whl?", digest.as_str());
    for position in 0..=crate::archive::MAX_CONTAINER_DEPTH {
        if position > 0 {
            uri.push('&');
        }
        uri.push_str("container=inner.zip");
    }

    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("exceeds the configured limit"));
}

#[tokio::test]
async fn test_inspect_archive_listing_limit_is_payload_too_large() {
    let h = harness().await;
    let mut buf = Vec::new();
    {
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default();
        for position in 0..=crate::archive::MAX_LISTED_ENTRIES {
            zip.start_file(format!("pkg/file-{position}.py"), options).unwrap();
            zip.write_all(b"").unwrap();
        }
        zip.finish().unwrap();
    }
    let digest = put_local_file(&h.state, "velodexpkg-1.0-py3-none-any.whl", &buf, "1.0");
    let uri = format!("/local/inspect/{}/velodexpkg-1.0-py3-none-any.whl", digest.as_str());

    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert!(body.contains("archive listing exceeds"));
}

#[tokio::test]
async fn test_inspect_bad_digest_and_missing_paths() {
    let h = harness().await;
    let (status, _, body) = get(&h.state, "/local/inspect/nothex/x.whl", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("expected 64 lowercase hex sha256"));
    let (status, ..) = get(&h.state, "/local/inspect/onlyonesegment", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let uri = format!("/local/inspect/{}/pkg%2Fname.whl", "a".repeat(64));
    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("filenames must be relative path segments"));
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
    put_local_file(&h.state, "velodexpkg.whl", b"payload", "9.9");
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
        put_local_file(&h.state, filename, format!("payload {version}").as_bytes(), version);
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
