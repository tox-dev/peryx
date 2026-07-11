//! The pypi.org-shaped legacy JSON API.

use super::support::*;

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
#[rstest]
#[case::invalid_project_path("/pypi/%FF/json", "invalid percent-encoded path segment")]
#[case::invalid_versioned_project_path("/pypi/%FF/1.0/json", "invalid percent-encoded path segment")]
#[case::invalid_version_path("/pypi/flask/%FF/json", "invalid percent-encoded path segment")]
#[case::unsafe_project_path("/pypi/flask%2Fbad/json", "invalid project \"flask/bad\"")]
#[case::unsafe_versioned_project_path("/pypi/flask%2Fbad/1.0/json", "invalid project \"flask/bad\"")]
#[case::unsafe_version_path("/pypi/flask/1.0%2Fbad/json", "invalid version \"1.0/bad\"")]
#[tokio::test]
async fn test_legacy_json_rejects_invalid_paths(#[case] uri: &str, #[case] expected: &str) {
    let h = harness().await;
    let (status, _, body) = get(&h.state, uri, None).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains(expected), "{body}");
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
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let upstream = UpstreamClient::new("http://127.0.0.1:0/simple/").unwrap();
    let state = crate::tests::wired(AppState::new(
        meta,
        blobs,
        60,
        vec![Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Cached {
                client: upstream,
                offline: false,
            },
            policy: Policy::default(),
        }],
    ));

    let (status, _, body) = get(&state, "/pypi/flask/json", None).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains("project detail on index \"pypi\" for project \"flask\""));
}
#[tokio::test]
async fn test_legacy_json_is_cached_per_version() {
    let h = harness().await;
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_detail(&h.server, Digest::of(b"wheel-v1").as_str(), &file_url, None).await;

    let (status, _, project) = get(&h.state, "/pypi/flask/json", None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, release) = get(&h.state, "/pypi/flask/1.0/json", None).await;
    assert_eq!(status, StatusCode::OK);
    // The release document is not the project document; one cache key cannot hold both.
    assert_ne!(project, release);

    h.server.reset().await;
    assert_eq!(get(&h.state, "/pypi/flask/json", None).await.2, project);
    assert_eq!(get(&h.state, "/pypi/flask/1.0/json", None).await.2, release);
}
#[tokio::test]
async fn test_index_response_error_is_bad_gateway() {
    use crate::cache::CacheError;
    use crate::serving::{Format, index_response};
    let response = index_response(Err(CacheError::Unavailable), Format::Json, "pypi");
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        String::from_utf8_lossy(&body),
        "project list on index \"pypi\": upstream is unavailable and no cached page exists"
    );
}
