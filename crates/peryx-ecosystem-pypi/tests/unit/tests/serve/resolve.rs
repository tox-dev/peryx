use super::support::*;
use crate::policy::FallbackMode;
use crate::tests::http::{LogCapture, field, policy, put_local_project};
use peryx_driver::serving::BrowseDriver as _;
use peryx_identity::IndexAcl;

fn nested_flask_page(digest: &str) -> String {
    format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\
         \"url\":\"https://upstream.invalid/flask-1.0-py3-none-any.whl\",\
         \"hashes\":{{\"sha256\":\"{digest}\"}}}}]}}"
    )
}

#[tokio::test]
async fn test_overlay_project_missing_everywhere_is_not_found() {
    let h = harness().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&h.server)
        .await;
    let (status, ..) = get(&h.state, "/root/pypi/simple/ghost/", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_resolve_detail_rejects_a_persisted_virtual_cycle() {
    let dir = tempfile::tempdir().unwrap();
    let state = custom_state(&dir, "https://example.invalid/simple/", |_client| {
        vec![
            runtime_index(
                "a",
                IndexKind::Virtual {
                    layers: vec![1],
                    write_target: None,
                },
            ),
            runtime_index(
                "b",
                IndexKind::Virtual {
                    layers: vec![0],
                    write_target: None,
                },
            ),
        ]
    });

    let (status, _, body) = get(&state, "/a/simple/flask/", Some("application/json")).await;

    assert_eq!(
        (status, body),
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "project detail on index \"a\" for project \"flask\": virtual index composition cycle: a -> b -> a"
                .to_owned(),
        )
    );
}

#[tokio::test]
async fn test_resolve_detail_allows_a_shared_virtual_descendant() {
    let dir = tempfile::tempdir().unwrap();
    let state = custom_state(&dir, "https://example.invalid/simple/", |_client| {
        vec![
            runtime_index("hosted", IndexKind::Hosted { volatile: true }),
            runtime_index(
                "left",
                IndexKind::Virtual {
                    layers: vec![0],
                    write_target: None,
                },
            ),
            runtime_index(
                "right",
                IndexKind::Virtual {
                    layers: vec![0],
                    write_target: None,
                },
            ),
            runtime_index(
                "root",
                IndexKind::Virtual {
                    layers: vec![1, 2],
                    write_target: Some(0),
                },
            ),
        ]
    });
    put_local_project(&state, "flask", "flask-1.0-py3-none-any.whl", b"wheel", "1.0");

    let detail = cache::resolve_detail(&state.serving, state.serving.index_at(3), "flask", "root")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        (
            detail.name,
            detail.versions,
            detail.files.into_iter().map(|file| file.filename).collect::<Vec<_>>(),
        ),
        (
            "flask".to_owned(),
            vec!["1.0".to_owned()],
            vec!["flask-1.0-py3-none-any.whl".to_owned()],
        )
    );
}

fn runtime_index(name: &str, kind: IndexKind) -> Index {
    Index {
        name: name.to_owned(),
        route: name.to_owned(),
        ecosystem: crate::ECOSYSTEM,
        kind,
        policy: Policy::default(),
        acl: IndexAcl::default(),
    }
}

#[tokio::test]
async fn test_inspect_fetches_an_uncached_file_from_upstream() {
    let h = harness().await;
    let wheel = b"not a real archive";
    let digest = Digest::of(wheel);
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_json_page(&h.server, &detail_json(digest.as_str(), &file_url)).await;
    Mock::given(method("GET"))
        .and(path("/files/flask.whl"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(wheel.to_vec()))
        .mount(&h.server)
        .await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    let uri = format!("/pypi/inspect/{}/flask-1.0-py3-none-any.whl", digest.as_str());
    get(&h.state, &uri, None).await;
    assert!(h.state.serving.blobs.head(&digest).await.unwrap().is_some());
}
#[tokio::test]
async fn test_inspect_digest_mismatch_is_bad_gateway() {
    let h = harness().await;
    let digest = Digest::of(b"expected");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_json_page(&h.server, &detail_json(digest.as_str(), &file_url)).await;
    Mock::given(method("GET"))
        .and(path("/files/flask.whl"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"wrong".to_vec()))
        .mount(&h.server)
        .await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    let uri = format!("/pypi/inspect/{}/flask-1.0-py3-none-any.whl", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains("file download on index \"pypi\""));
    assert!(body.contains("flask-1.0-py3-none-any.whl"));
    assert!(body.contains(digest.as_str()));
    assert!(h.state.serving.blobs.head(&digest).await.unwrap().is_none());
}
#[test]
fn test_offline_missing_user_message_names_target() {
    assert_eq!(
        cache::CacheError::OfflineMissing("metadata").user_message(),
        "offline mode has no cached metadata"
    );
}
#[tokio::test]
async fn test_refresh_stale_pages_skips_offline_mirrors() {
    let dir = tempfile::tempdir().unwrap();
    let state = custom_state(&dir, "https://example.invalid/simple/", |client| {
        vec![Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Cached { client, offline: true },
            policy: peryx_policy::Policy::default(),
            acl: IndexAcl::default(),
        }]
    });
    state
        .serving
        .meta
        .put_index(
            "pypi/flask",
            &CachedIndex {
                etag: None,
                last_serial: None,
                fetched_at_unix: 0,
                content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
                fresh_secs: Some(1),
                body: detail_json(Digest::of(b"wheel").as_str(), "https://example.invalid/files/flask.whl")
                    .into_bytes(),
            },
        )
        .unwrap();

    let summary = cache::refresh_stale_pages(&state.serving).await.unwrap();

    assert_eq!(summary.checked, 0);
    assert_eq!(summary.changed, 0);
}
#[tokio::test]
async fn test_offline_metadata_fetches_are_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let state = custom_state(&dir, "https://example.invalid/simple/", |client| {
        vec![Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Cached { client, offline: true },
            policy: peryx_policy::Policy::default(),
            acl: IndexAcl::default(),
        }]
    });
    let artifact = Digest::of(b"wheel");
    let metadata = Digest::of(b"metadata");
    state
        .serving
        .meta
        .put_metadata(
            artifact.as_str(),
            "https://example.invalid/files/flask.whl.metadata",
            metadata.as_str(),
            "pypi",
        )
        .unwrap();

    let err = cache::metadata_bytes(&state.serving, &artifact, "pypi", "flask-1.0-py3-none-any.whl.metadata")
        .await
        .unwrap_err();

    assert!(matches!(err, cache::CacheError::OfflineMissing("metadata")));
}
#[tokio::test]
async fn test_offline_generated_wheel_metadata_range_fetch_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let state = custom_state(&dir, "https://example.invalid/simple/", |client| {
        vec![Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Cached { client, offline: true },
            policy: peryx_policy::Policy::default(),
            acl: IndexAcl::default(),
        }]
    });
    let artifact = Digest::of(b"wheel");
    state
        .serving
        .meta
        .put_file_url(
            artifact.as_str(),
            "https://example.invalid/files/flask-1.0-py3-none-any.whl",
            "pypi",
        )
        .unwrap();

    let err = cache::metadata_bytes(&state.serving, &artifact, "pypi", "flask-1.0-py3-none-any.whl.metadata")
        .await
        .unwrap_err();

    assert!(matches!(err, cache::CacheError::OfflineMissing("metadata")));
}
#[tokio::test]
async fn test_overlay_offline_cold_mirror_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let state = custom_state(&dir, "https://example.invalid/simple/", |client| {
        vec![
            Index {
                name: "pypi".to_owned(),
                route: "pypi".to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Cached { client, offline: true },
                policy: peryx_policy::Policy::default(),
                acl: IndexAcl::default(),
            },
            Index {
                name: "root-pypi".to_owned(),
                route: "root/pypi".to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Virtual {
                    layers: vec![0],
                    write_target: None,
                },
                policy: peryx_policy::Policy::default(),
                acl: IndexAcl::default(),
            },
        ]
    });

    let (status, _, body) = get(&state, "/root/pypi/simple/flask/", None).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("offline mode has no cached project page"));
}
#[tokio::test]
async fn test_offline_mirror_resolves_cached_page() {
    let dir = tempfile::tempdir().unwrap();
    let state = custom_state(&dir, "https://example.invalid/simple/", |client| {
        vec![Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Cached { client, offline: true },
            policy: peryx_policy::Policy::default(),
            acl: IndexAcl::default(),
        }]
    });
    state
        .serving
        .meta
        .put_index(
            "pypi/flask",
            &fresh_record(
                &detail_json(Digest::of(b"wheel").as_str(), "https://example.invalid/files/flask.whl").into_bytes(),
            ),
        )
        .unwrap();

    let detail = cache::resolve_detail(&state.serving, state.serving.index_at(0), "flask", "pypi")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(detail.name, "flask");
}

#[tokio::test]
async fn test_buffered_resolution_uses_the_upstream_route() {
    let first = MockServer::start().await;
    let second = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&first)
        .await;
    mount_json_page(
        &second,
        &detail_json(Digest::of(b"wheel").as_str(), "https://example.invalid/flask.whl"),
    )
    .await;
    let primary = UpstreamClient::new(&format!("{}/simple/", first.uri())).unwrap();
    let router = UpstreamRouter::new(vec![
        NamedUpstream::new("first", primary.clone()),
        NamedUpstream::new(
            "second",
            UpstreamClient::new(&format!("{}/simple/", second.uri())).unwrap(),
        ),
    ])
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let state = routed_state(&dir, primary, router);

    let detail = cache::resolve_detail(&state.serving, state.serving.index_at(0), "flask", "pypi")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(detail.name, "flask");
    assert_eq!(second.received_requests().await.unwrap().len(), 1);
    assert_eq!(
        state
            .serving
            .meta
            .get_file_url(Digest::of(b"wheel").as_str())
            .unwrap()
            .unwrap()
            .upstream
            .as_deref(),
        Some("second")
    );
}

#[tokio::test]
async fn test_overlay_with_two_mirrors_serves_buffered() {
    let server = MockServer::start().await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", server.uri());
    let page = format!(
        "{{\"meta\":{{\"api-version\":\"1.4\",\"project-status\":\"archived\",\
         \"project-status-reason\":\"read only\"}},\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\"url\":\"{file_url}\",\
         \"hashes\":{{\"sha256\":\"{digest}\"}}}}]}}",
        digest = digest.as_str(),
    );
    mount_json_page(&server, &page).await;
    let dir = tempfile::tempdir().unwrap();
    let state = custom_state(&dir, &format!("{}/simple/", server.uri()), |client| {
        vec![
            Index {
                name: "a".to_owned(),
                route: "a".to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Cached {
                    client: client.clone(),
                    offline: false,
                },
                policy: peryx_policy::Policy::default(),
                acl: IndexAcl::default(),
            },
            Index {
                name: "b".to_owned(),
                route: "b".to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Cached { client, offline: false },
                policy: peryx_policy::Policy::default(),
                acl: IndexAcl::default(),
            },
            Index {
                name: "both".to_owned(),
                route: "both".to_owned(),
                policy: peryx_policy::Policy::default(),
                acl: IndexAcl::default(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Virtual {
                    layers: vec![0, 1],
                    write_target: None,
                },
            },
        ]
    });
    let (status, _, body) = get(&state, "/both/simple/flask/", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(digest.as_str()));
    assert!(body.contains(r#""project-status":"archived""#));
    assert!(body.contains(r#""project-status-reason":"read only""#));
}
#[tokio::test]
async fn test_overlay_nesting_an_overlay_serves_buffered() {
    let server = MockServer::start().await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", server.uri());
    mount_json_page(&server, &detail_json(digest.as_str(), &file_url)).await;
    let dir = tempfile::tempdir().unwrap();
    let state = custom_state(&dir, &format!("{}/simple/", server.uri()), |client| {
        vec![
            Index {
                name: "a".to_owned(),
                route: "a".to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Cached { client, offline: false },
                policy: peryx_policy::Policy::default(),
                acl: IndexAcl::default(),
            },
            Index {
                name: "inner".to_owned(),
                route: "inner".to_owned(),
                policy: peryx_policy::Policy::default(),
                acl: IndexAcl::default(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Virtual {
                    layers: vec![0],
                    write_target: None,
                },
            },
            Index {
                name: "outer".to_owned(),
                route: "outer".to_owned(),
                policy: peryx_policy::Policy::default(),
                acl: IndexAcl::default(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Virtual {
                    layers: vec![1],
                    write_target: None,
                },
            },
        ]
    });
    let (status, _, body) = get(&state, "/outer/simple/flask/", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(digest.as_str()));
}
#[tokio::test]
async fn test_overlay_without_a_mirror_serves_buffered() {
    let dir = tempfile::tempdir().unwrap();
    let state = custom_state(&dir, "https://unused.invalid/simple/", |_| {
        vec![
            Index {
                name: "hosted".to_owned(),
                route: "hosted".to_owned(),
                policy: peryx_policy::Policy::default(),
                acl: IndexAcl::default(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Hosted { volatile: true },
            },
            Index {
                name: "only".to_owned(),
                route: "only".to_owned(),
                policy: peryx_policy::Policy::default(),
                acl: IndexAcl::default(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Virtual {
                    layers: vec![0],
                    write_target: Some(0),
                },
            },
        ]
    });
    let (status, ..) = get(&state, "/only/simple/ghost/", Some("application/json")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
#[tokio::test]
async fn test_stats_endpoint_drills_by_index_and_project() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_json_page(&h.server, &detail_json(digest.as_str(), &file_url)).await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    h.state.serving.metrics.flush().unwrap();

    let authorization = crate::tests::administrator_header(&h.state).await;
    let credentials = [(axum::http::header::AUTHORIZATION.as_str(), authorization.as_str())];
    let (status, _, bytes) = crate::tests::http::get_bytes_with_headers(&h.state, "/+stats", &credentials).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&bytes).contains("pypi"));
    let (status, _, bytes) =
        crate::tests::http::get_bytes_with_headers(&h.state, "/+stats?repository=pypi&resource=flask", &credentials)
            .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        String::from_utf8_lossy(&bytes).contains("artifacts"),
        "{}",
        String::from_utf8_lossy(&bytes)
    );
}
#[tokio::test]
async fn test_upstream_file_error_is_bad_gateway() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_json_page(&h.server, &detail_json(digest.as_str(), &file_url)).await;
    Mock::given(method("GET"))
        .and(path("/files/flask.whl"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&h.server)
        .await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    let uri = format!("/pypi/files/{}/flask-1.0-py3-none-any.whl", digest.as_str());
    let (status, ..) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}
#[tokio::test]
async fn test_upstream_metadata_error_is_bad_gateway() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    let page = format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\"url\":\"{file_url}\",\
         \"hashes\":{{\"sha256\":\"{digest}\"}},\"core-metadata\":{{\"sha256\":\"{meta}\"}}}}]}}",
        digest = digest.as_str(),
        meta = Digest::of(b"meta").as_str(),
    );
    mount_json_page(&h.server, &page).await;
    Mock::given(method("GET"))
        .and(path("/files/flask.whl.metadata"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&h.server)
        .await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    let uri = format!("/pypi/files/{}/flask-1.0-py3-none-any.whl.metadata", digest.as_str());
    let (status, ..) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}
#[tokio::test]
async fn test_upstream_metadata_404_is_negative_cached() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    let page = format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\"url\":\"{file_url}\",\
         \"hashes\":{{\"sha256\":\"{digest}\"}},\"core-metadata\":{{\"sha256\":\"{meta}\"}}}}]}}",
        digest = digest.as_str(),
        meta = Digest::of(b"meta").as_str(),
    );
    mount_json_page(&h.server, &page).await;
    Mock::given(method("GET"))
        .and(path("/files/flask.whl.metadata"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&h.server)
        .await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    let uri = format!("/pypi/files/{}/flask-1.0-py3-none-any.whl.metadata", digest.as_str());

    let first = get(&h.state, &uri, None).await;
    let second = get(&h.state, &uri, None).await;

    assert_eq!((first.0, second.0), (StatusCode::NOT_FOUND, StatusCode::NOT_FOUND));
}
#[tokio::test]
async fn test_foreign_index_rejects_pypi_protocol_dispatch() {
    use axum::body::Body;
    use axum::http::{Method, Request, header};
    use peryx_http::router;
    use tower::ServiceExt as _;

    let dir = tempfile::tempdir().unwrap();
    let state = custom_state(&dir, "http://127.0.0.1:9/simple/", |_client| {
        vec![Index {
            name: "foreign".to_owned(),
            route: "foreign".to_owned(),
            ecosystem: peryx_core::Ecosystem::new("foreign"),
            kind: IndexKind::Hosted { volatile: true },
            policy: Policy::default(),
            acl: crate::tests::writer_acl("s3cret".to_owned()),
        }]
    });
    assert_eq!(get(&state, "/foreign/simple/x/", None).await.0, StatusCode::NOT_FOUND);
    let auth = crate::tests::http::upload_auth();
    for method in [Method::PUT, Method::DELETE] {
        let request = Request::builder()
            .method(method.clone())
            .uri("/foreign/x/1.0/yank")
            .header(header::AUTHORIZATION, &auth)
            .body(Body::empty())
            .unwrap();
        let response = router(state.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{method}");
    }
    let (content_type, body) = crate::tests::http::multipart_body(&[("name", "x"), ("version", "1.0")], None);
    assert_eq!(
        crate::tests::http::post_upload(&state, "/foreign/", Some(&auth), &content_type, body).await,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn test_no_fallback_denies_a_cache_reached_through_a_nested_virtual() {
    let server = MockServer::start().await;
    mount_json_page(&server, &nested_flask_page(Digest::of(b"upstream flask").as_str())).await;
    let dir = tempfile::tempdir().unwrap();
    let state = custom_state(&dir, &format!("{}/simple/", server.uri()), |client| {
        vec![
            Index {
                name: "pypi".to_owned(),
                route: "pypi".to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Cached { client, offline: false },
                policy: Policy::default(),
                acl: IndexAcl::default(),
            },
            Index {
                name: "hosted".to_owned(),
                route: "hosted".to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Hosted { volatile: true },
                policy: Policy::default(),
                acl: IndexAcl::default(),
            },
            Index {
                name: "inner".to_owned(),
                route: "inner".to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Virtual {
                    layers: vec![0],
                    write_target: None,
                },
                policy: Policy::default(),
                acl: IndexAcl::default(),
            },
            Index {
                name: "outer".to_owned(),
                route: "outer".to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Virtual {
                    layers: vec![1, 2],
                    write_target: None,
                },
                policy: policy(|_neutral, pypi| pypi.fallback_mode = FallbackMode::NoFallback),
                acl: IndexAcl::default(),
            },
        ]
    });

    let (status, _, body) = get(&state, "/outer/simple/flask/", Some("application/json")).await;

    let denial: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denial["rule"], "virtual-fallback");
    let reason = denial["reason"].as_str().unwrap();
    assert!(reason.contains("no-fallback"), "{reason}");
    assert!(reason.contains("hosted"), "{reason}");
    assert!(reason.contains("inner"), "{reason}");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_no_fallback_denies_a_cache_reached_through_several_virtual_layers() {
    let server = MockServer::start().await;
    mount_json_page(&server, &nested_flask_page(Digest::of(b"upstream flask").as_str())).await;
    let dir = tempfile::tempdir().unwrap();
    let state = custom_state(&dir, &format!("{}/simple/", server.uri()), |client| {
        vec![
            Index {
                name: "pypi".to_owned(),
                route: "pypi".to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Cached { client, offline: false },
                policy: Policy::default(),
                acl: IndexAcl::default(),
            },
            Index {
                name: "inner".to_owned(),
                route: "inner".to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Virtual {
                    layers: vec![0],
                    write_target: None,
                },
                policy: Policy::default(),
                acl: IndexAcl::default(),
            },
            Index {
                name: "middle".to_owned(),
                route: "middle".to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Virtual {
                    layers: vec![1],
                    write_target: None,
                },
                policy: Policy::default(),
                acl: IndexAcl::default(),
            },
            Index {
                name: "outer".to_owned(),
                route: "outer".to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Virtual {
                    layers: vec![2],
                    write_target: None,
                },
                policy: policy(|_neutral, pypi| pypi.fallback_mode = FallbackMode::NoFallback),
                acl: IndexAcl::default(),
            },
        ]
    });

    let (status, _, body) = get(&state, "/outer/simple/flask/", Some("application/json")).await;

    let denial: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denial["rule"], "virtual-fallback");
    assert!(denial["reason"].as_str().unwrap().contains("middle"), "{body}");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_protected_name_blocks_a_cache_reached_through_a_nested_virtual() {
    let server = MockServer::start().await;
    mount_json_page(&server, &nested_flask_page(Digest::of(b"upstream flask").as_str())).await;
    let dir = tempfile::tempdir().unwrap();
    let state = custom_state(&dir, &format!("{}/simple/", server.uri()), |client| {
        vec![
            Index {
                name: "pypi".to_owned(),
                route: "pypi".to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Cached { client, offline: false },
                policy: Policy::default(),
                acl: IndexAcl::default(),
            },
            Index {
                name: "inner".to_owned(),
                route: "inner".to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Virtual {
                    layers: vec![0],
                    write_target: None,
                },
                policy: Policy::default(),
                acl: IndexAcl::default(),
            },
            Index {
                name: "outer".to_owned(),
                route: "outer".to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Virtual {
                    layers: vec![1],
                    write_target: None,
                },
                policy: policy(|neutral, _pypi| neutral.protected_resources = vec!["flask".to_owned()]),
                acl: IndexAcl::default(),
            },
        ]
    });

    let (status, _, body) = get(&state, "/outer/simple/flask/", Some("application/json")).await;

    let denial: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(denial["rule"], "protected-name");
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn test_private_first_shadows_a_cache_reached_through_a_nested_virtual() {
    let server = MockServer::start().await;
    mount_json_page(&server, &nested_flask_page(Digest::of(b"upstream flask").as_str())).await;
    let dir = tempfile::tempdir().unwrap();
    let state = custom_state(&dir, &format!("{}/simple/", server.uri()), |client| {
        vec![
            Index {
                name: "pypi".to_owned(),
                route: "pypi".to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Cached { client, offline: false },
                policy: Policy::default(),
                acl: IndexAcl::default(),
            },
            Index {
                name: "hosted".to_owned(),
                route: "hosted".to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Hosted { volatile: true },
                policy: Policy::default(),
                acl: IndexAcl::default(),
            },
            Index {
                name: "inner".to_owned(),
                route: "inner".to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Virtual {
                    layers: vec![0],
                    write_target: None,
                },
                policy: Policy::default(),
                acl: IndexAcl::default(),
            },
            Index {
                name: "outer".to_owned(),
                route: "outer".to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Virtual {
                    layers: vec![1, 2],
                    write_target: None,
                },
                policy: policy(|_neutral, pypi| pypi.fallback_mode = FallbackMode::PrivateFirst),
                acl: IndexAcl::default(),
            },
        ]
    });
    put_local_project(&state, "flask", "flask-9.9-py3-none-any.whl", b"hosted flask", "9.9");
    let logs = LogCapture::default();
    let guard = logs.install();

    let (status, _, body) = get(&state, "/outer/simple/flask/", Some("application/json")).await;

    drop(guard);
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("flask-9.9-py3-none-any.whl"), "{body}");
    assert!(!body.contains("flask-1.0-py3-none-any.whl"), "{body}");
    let event = logs
        .security_events()
        .into_iter()
        .find(|event| field(event, "event") == Some("policy_decision"))
        .unwrap();
    assert_eq!(field(&event, "result"), Some("shadowed"));
    assert_eq!(field(&event, "index"), Some("outer"));
    assert_eq!(field(&event, "hosted_members"), Some("hosted"));
    assert_eq!(field(&event, "cached_members"), Some("inner"));
}

#[tokio::test]
async fn test_project_page_reports_an_unreachable_upstream() {
    let dir = tempfile::tempdir().unwrap();
    let state = custom_state(&dir, "http://127.0.0.1:9/simple/", |client| {
        vec![Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Cached { client, offline: false },
            policy: peryx_policy::Policy::default(),
            acl: IndexAcl::default(),
        }]
    });
    let result = crate::serving::PypiServing
        .browse(state.serving.clone(), 0, "index=pypi&project=flask".to_owned())
        .await;
    assert!(result.is_err(), "{result:?}");
}
