//! Serving a cached index: fetch, revalidate, and the stale and offline fallbacks.

use super::support::*;
use peryx_identity::IndexAcl;

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
async fn test_persist_page_skips_policy_denied_file_registrations() {
    let mirror_policy = policy(|_neutral, pypi| {
        pypi.block_package_types = vec![PackageType::Wheel];
    });
    let h = harness_with_policies(true, true, mirror_policy, Policy::default(), Policy::default()).await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    let record = CachedIndex {
        etag: None,
        last_serial: None,
        fetched_at_unix: 1000,
        content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
        fresh_secs: None,
        body: detail_json(digest.as_str(), &file_url).into_bytes(),
    };

    cache::persist_page(&h.state, "pypi/flask", "pypi", "flask", &record).unwrap();

    assert!(h.state.meta.get_file_url(digest.as_str()).unwrap().is_none());
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
async fn test_mirror_detail_refuses_a_page_staler_than_the_bound() {
    // Fetched at 0, clock at 1000: past the 60s freshness and the 300s stale bound alike.
    let h = stale_page_harness(300, 0).await;
    let (status, _, _) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    assert_ne!(status, StatusCode::OK);
}
#[tokio::test]
async fn test_mirror_detail_serves_any_age_when_the_bound_is_zero() {
    // The same ancient page, with the bound switched off: an operator mirroring an unreliable
    // upstream asked for exactly this.
    let h = stale_page_harness(0, 0).await;
    let (status, _, served) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(served.contains("flask"));
}
#[tokio::test]
async fn test_mirror_detail_stale_on_5xx() {
    let h = harness().await;
    let body = crate::to_json(&crate::ProjectDetail {
        meta: crate::Meta::default(),
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
                fetched_at_unix: 900,
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
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let upstream = UpstreamClient::new("http://127.0.0.1:0/simple/").unwrap();
    let indexes = vec![Index {
        name: "pypi".to_owned(),
        route: "pypi".to_owned(),
        ecosystem: peryx_core::Ecosystem::Pypi,
        kind: IndexKind::Cached {
            client: upstream,
            offline: false,
        },
        policy: Policy::default(),
        acl: IndexAcl::default(),
    }];
    let state = crate::tests::wired(AppState::new(meta, blobs, 60, indexes));
    let (status, ..) = get(&state, "/pypi/simple/flask/", None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}
#[tokio::test]
async fn test_mirror_detail_stale_on_upstream_error() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let upstream = UpstreamClient::new("http://127.0.0.1:0/simple/").unwrap();
    let body = crate::to_json(&crate::ProjectDetail {
        meta: crate::Meta::default(),
        name: "flask".to_owned(),
        versions: vec![],
        files: vec![],
    });
    meta.put_index(
        "pypi/flask",
        &CachedIndex {
            etag: None,
            last_serial: None,
            fetched_at_unix: 99_900,
            content_type: None,

            fresh_secs: None,
            body: body.into_bytes(),
        },
    )
    .unwrap();
    let indexes = vec![Index {
        name: "pypi".to_owned(),
        route: "pypi".to_owned(),
        ecosystem: peryx_core::Ecosystem::Pypi,
        kind: IndexKind::Cached {
            client: upstream,
            offline: false,
        },
        policy: Policy::default(),
        acl: IndexAcl::default(),
    }];
    let state = crate::tests::wired(AppState::with_clock(meta, blobs, 60, indexes, Arc::new(|| 100_000)));
    let (status, _, served) = get(&state, "/pypi/simple/flask/", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(served.contains("flask"));
}
#[tokio::test]
async fn test_offline_mirror_cold_project_miss_is_unavailable() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let upstream = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let indexes = vec![Index {
        name: "pypi".to_owned(),
        route: "pypi".to_owned(),
        ecosystem: peryx_core::Ecosystem::Pypi,
        kind: IndexKind::Cached {
            client: upstream,
            offline: true,
        },
        policy: Policy::default(),
        acl: IndexAcl::default(),
    }];
    let state = crate::tests::wired(AppState::new(meta, blobs, 60, indexes));
    let (status, _, body) = get(&state, "/pypi/simple/flask/", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("offline mode has no cached project page"));
}
#[tokio::test]
async fn test_offline_mirror_serves_stale_cached_page() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let body = crate::to_json(&crate::ProjectDetail {
        meta: crate::Meta::default(),
        name: "flask".to_owned(),
        versions: vec!["1.0".to_owned()],
        files: vec![],
    });
    meta.put_index(
        "pypi/flask",
        &CachedIndex {
            etag: None,
            last_serial: None,
            fetched_at_unix: 0,
            content_type: None,
            fresh_secs: Some(1),
            body: body.into_bytes(),
        },
    )
    .unwrap();
    let upstream = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let indexes = vec![Index {
        name: "pypi".to_owned(),
        route: "pypi".to_owned(),
        ecosystem: peryx_core::Ecosystem::Pypi,
        kind: IndexKind::Cached {
            client: upstream,
            offline: true,
        },
        policy: Policy::default(),
        acl: IndexAcl::default(),
    }];
    let state = crate::tests::wired(AppState::with_clock(meta, blobs, 60, indexes, Arc::new(|| 100_000)));
    let (status, _, body) = get(&state, "/pypi/simple/flask/", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"name\":\"flask\""));
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
