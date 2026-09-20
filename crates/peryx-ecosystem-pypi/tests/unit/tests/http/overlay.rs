use super::support::*;
use crate::policy::FallbackMode;
use peryx_identity::IndexAcl;
use peryx_test_support::fault::{backend, faulted};

async fn publish_peryxpkg(harness: &Harness) {
    upload_wheel(&harness.state, "peryxpkg-1.0-py3-none-any.whl", &fixture_wheel()).await;
}

async fn mount_upstream_peryxpkg(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/simple/peryxpkg/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            b"{\"meta\":{\"api-version\":\"1.1\"},\"name\":\"peryxpkg\",\"versions\":[\"1.0\"],\"files\":[{\"filename\":\"peryxpkg-1.0-py3-none-any.whl\",\"size\":11,\"url\":\"https://upstream.invalid/peryxpkg-1.0-py3-none-any.whl\",\"hashes\":{\"sha256\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}}]}".to_vec(),
            "application/vnd.pypi.simple.v1+json",
        ))
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn test_overlay_serves_buffered_when_mirror_layer_policy_is_active() {
    let mirror_policy = policy(|neutral, _pypi| {
        neutral.block_resources = vec!["blocked".to_owned()];
    });
    let h = harness_with_policies(true, true, mirror_policy, Policy::default(), Policy::default()).await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_detail(&h.server, digest.as_str(), &file_url, None).await;

    let (status, _, body) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(digest.as_str()));
}
#[tokio::test]
async fn test_overlay_serves_buffered_when_local_layer_policy_is_active() {
    let local_policy = policy(|neutral, _pypi| {
        neutral.block_resources = vec!["blocked".to_owned()];
    });
    let h = harness_with_policies(true, true, Policy::default(), local_policy, Policy::default()).await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_detail(&h.server, digest.as_str(), &file_url, None).await;

    let (status, _, body) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(digest.as_str()));
}
#[tokio::test]
async fn test_overlay_rejects_an_unavailable_layer_with_a_hosted_candidate() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let upstream = UpstreamClient::new("http://127.0.0.1:0/simple/").unwrap();
    let indexes = vec![
        Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Cached {
                client: upstream,
                offline: false,
            },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        },
        Index {
            name: "hosted".to_owned(),
            route: "hosted".to_owned(),
            policy: Policy::default(),
            acl: crate::tests::writer_acl("s3cret".to_owned()),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Hosted { volatile: true },
        },
        Index {
            name: "root-pypi".to_owned(),
            route: "root/pypi".to_owned(),
            policy: Policy::default(),
            acl: IndexAcl::default(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Virtual {
                layers: vec![1, 0],
                write_target: Some(1),
            },
        },
    ];
    let state = crate::tests::wired(AppState::new(meta, blobs, 60, indexes));
    upload_peryxpkg(&state, "/root/pypi/", &fixture_wheel()).await;

    let (status, _, detail) = get(&state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(detail.contains("upstream connection failed"), "{detail}");
}

#[rstest]
#[case::unauthorized(401, StatusCode::BAD_GATEWAY, "application/json")]
#[case::forbidden(403, StatusCode::BAD_GATEWAY, "text/html")]
#[case::server_error(500, StatusCode::BAD_GATEWAY, "application/json")]
#[tokio::test]
async fn test_overlay_surfaces_an_upstream_failure_with_a_hosted_candidate(
    #[case] upstream_status: u16,
    #[case] expected_status: StatusCode,
    #[case] accept: &str,
) {
    let harness = harness().await;
    Mock::given(method("GET"))
        .and(path("/simple/peryxpkg/"))
        .respond_with(ResponseTemplate::new(upstream_status))
        .mount(&harness.server)
        .await;
    publish_peryxpkg(&harness).await;

    let (status, _, body) = get(&harness.state, "/root/pypi/simple/peryxpkg/", Some(accept)).await;

    assert_eq!(status, expected_status);
    assert!(
        body.contains("upstream is unavailable and no cached page exists"),
        "{body}"
    );
}

#[tokio::test]
async fn test_overlay_surfaces_a_malformed_simple_page_with_a_hosted_candidate() {
    let harness = harness().await;
    Mock::given(method("GET"))
        .and(path("/simple/peryxpkg/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(b"not a simple page".to_vec(), "text/plain"))
        .mount(&harness.server)
        .await;
    publish_peryxpkg(&harness).await;

    let (status, _, body) = get(&harness.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains("upstream returned an invalid response"), "{body}");
}

#[tokio::test]
async fn test_overlay_surfaces_an_offline_miss_with_a_hosted_candidate() {
    let harness = offline_harness(Policy::default()).await;
    publish_peryxpkg(&harness).await;

    let (status, _, body) = get(&harness.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.contains("offline mode has no cached project page"), "{body}");
}

#[tokio::test]
async fn test_overlay_forwards_upstream_retry_after_with_a_hosted_candidate() {
    let harness = harness().await;
    Mock::given(method("GET"))
        .and(path("/simple/peryxpkg/"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "120"))
        .mount(&harness.server)
        .await;
    publish_peryxpkg(&harness).await;

    let (status, headers, body) = get(&harness.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;

    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(headers[header::RETRY_AFTER].to_str().unwrap(), "120");
    assert!(body.contains("upstream rate limit exceeded"), "{body}");
}

#[tokio::test]
async fn test_overlay_surfaces_a_nested_cached_member_failure() {
    let harness = nested_harness(Policy::default()).await;
    Mock::given(method("GET"))
        .and(path("/simple/peryxpkg/"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&harness.server)
        .await;
    publish_peryxpkg(&harness).await;

    let (status, _, body) = get(&harness.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(
        body.contains("upstream is unavailable and no cached page exists"),
        "{body}"
    );
}

#[tokio::test]
async fn test_resolve_detail_surfaces_a_member_failure_with_a_hosted_candidate() {
    let harness = harness().await;
    Mock::given(method("GET"))
        .and(path("/simple/peryxpkg/"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&harness.server)
        .await;
    publish_peryxpkg(&harness).await;

    let error = cache::resolve_detail(
        &harness.state.serving,
        harness.state.serving.index_at(2),
        "peryxpkg",
        "root/pypi",
    )
    .await
    .unwrap_err();

    assert!(matches!(error, cache::CacheError::Unavailable));
}

#[tokio::test]
async fn test_overlay_surfaces_corrupt_hosted_metadata_before_a_cached_collision() {
    let harness = harness().await;
    mount_upstream_peryxpkg(&harness.server).await;
    harness
        .state
        .serving
        .meta
        .put_upload("hosted", "peryxpkg", "peryxpkg-1.0-py3-none-any.whl", b"not json")
        .unwrap();

    let (status, _, body) = get(&harness.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
}

#[tokio::test]
async fn test_private_first_nested_overlay_surfaces_corrupt_hosted_metadata_before_a_cached_collision() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let upstream = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let state = crate::tests::wired(AppState::new(
        meta,
        blobs,
        60,
        vec![
            Index {
                name: "pypi".to_owned(),
                route: "pypi".to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Cached {
                    client: upstream,
                    offline: false,
                },
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
                    layers: vec![1],
                    write_target: None,
                },
                policy: Policy::default(),
                acl: IndexAcl::default(),
            },
            Index {
                name: "root".to_owned(),
                route: "root".to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Virtual {
                    layers: vec![2, 0],
                    write_target: None,
                },
                policy: policy(|_, pypi| pypi.fallback_mode = FallbackMode::PrivateFirst),
                acl: IndexAcl::default(),
            },
        ],
    ));
    mount_upstream_peryxpkg(&server).await;
    state
        .serving
        .meta
        .put_upload("hosted", "peryxpkg", "peryxpkg-1.0-py3-none-any.whl", b"not json")
        .unwrap();

    let (status, _, body) = get(&state, "/root/simple/peryxpkg/", Some("application/json")).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
}

#[tokio::test]
async fn test_overlay_treats_an_empty_upstream_page_as_a_successful_member() {
    let harness = harness().await;
    Mock::given(method("GET"))
        .and(path("/simple/peryxpkg/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            b"{\"meta\":{\"api-version\":\"1.1\"},\"name\":\"peryxpkg\",\"versions\":[],\"files\":[]}".to_vec(),
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(&harness.server)
        .await;
    let (status, _, body) = get(&harness.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["files"],
        serde_json::json!([])
    );
}

#[tokio::test]
async fn test_resolve_detail_surfaces_a_faulted_hosted_read_before_a_cached_collision() {
    let (inner, fault) = backend();
    let meta = MetaStore::open_backend(faulted(&inner, &fault)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let state = crate::tests::wired(AppState::new(
        meta,
        BlobStorage::filesystem(dir.path().join("blobs")),
        60,
        vec![
            Index {
                name: "pypi".to_owned(),
                route: "pypi".to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Cached {
                    client: UpstreamClient::new("http://127.0.0.1:9/simple/").unwrap(),
                    offline: true,
                },
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
                name: "root".to_owned(),
                route: "root".to_owned(),
                ecosystem: crate::ECOSYSTEM,
                kind: IndexKind::Virtual {
                    layers: vec![1, 0],
                    write_target: None,
                },
                policy: Policy::default(),
                acl: IndexAcl::default(),
            },
        ],
    ));
    put_local_project(&state, "peryxpkg", "peryxpkg-1.0-py3-none-any.whl", b"wheel", "1.0");
    state
        .serving
        .meta
        .put_index(
            "pypi/peryxpkg",
            &CachedIndex {
                source: None,
                last_modified: None,
                etag: None,
                last_serial: None,
                fetched_at_unix: 1000,
                content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
                fresh_secs: None,
                body: b"{\"meta\":{\"api-version\":\"1.1\"},\"name\":\"peryxpkg\",\"versions\":[\"1.0\"],\"files\":[{\"filename\":\"peryxpkg-1.0-py3-none-any.whl\",\"size\":5,\"url\":\"https://upstream.invalid/peryxpkg-1.0-py3-none-any.whl\",\"hashes\":{\"sha256\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}}]}".to_vec(),
            },
        )
        .unwrap();

    assert!(
        cache::resolve_detail(&state.serving, state.serving.index_at(0), "peryxpkg", "pypi")
            .await
            .unwrap()
            .is_some()
    );
    fault.arm(0);
    let error = cache::resolve_detail(&state.serving, state.serving.index_at(2), "peryxpkg", "root")
        .await
        .unwrap_err();

    assert!(fault.triggered());
    assert!(matches!(error, cache::CacheError::Meta(_)));
}

#[tokio::test]
async fn test_overlay_selects_a_valid_stale_cached_page() {
    let harness = harness().await;
    harness
        .state
        .serving
        .meta
        .put_index(
            "pypi/peryxpkg",
            &CachedIndex {
                source: None,
                last_modified: None,
                etag: None,
                last_serial: None,
                fetched_at_unix: 900,
                content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
                fresh_secs: None,
                body: b"{\"meta\":{\"api-version\":\"1.1\"},\"name\":\"peryxpkg\",\"versions\":[\"1.0\"],\"files\":[{\"filename\":\"peryxpkg-1.0-py3-none-any.whl\",\"size\":11,\"url\":\"https://upstream.invalid/peryxpkg-1.0-py3-none-any.whl\",\"hashes\":{\"sha256\":\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"}}]}".to_vec(),
            },
        )
        .unwrap();
    Mock::given(method("GET"))
        .and(path("/simple/peryxpkg/"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&harness.server)
        .await;

    let (status, _, body) = get(&harness.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;

    assert_eq!(status, StatusCode::OK);
    assert!(
        body.contains("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        "{body}"
    );
}

#[tokio::test]
async fn test_overlay_upload_only_project_unknown_elsewhere() {
    let h = harness().await;

    Mock::given(method("GET"))
        .and(path("/simple/peryxpkg/"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&h.server)
        .await;
    upload_wheel(&h.state, "peryxpkg-1.0-py3-none-any.whl", &fixture_wheel()).await;
    for _ in 0..2 {
        let (status, _, detail) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(detail.contains("peryxpkg-1.0-py3-none-any.whl"));
    }
    assert_eq!(h.server.received_requests().await.unwrap().len(), 1);
}
#[tokio::test]
async fn test_overlay_without_upload_layer_serves_merged_page() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let upstream = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let digest = Digest::of(b"wheel");
    mount_detail(&server, digest.as_str(), "http://x/flask-1.0-py3-none-any.whl", None).await;
    let indexes = vec![
        Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Cached {
                client: upstream,
                offline: false,
            },
            policy: Policy::default(),
            acl: IndexAcl::default(),
        },
        Index {
            name: "ov".to_owned(),
            route: "ov".to_owned(),
            policy: Policy::default(),
            acl: IndexAcl::default(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Virtual {
                layers: vec![0],
                write_target: None,
            },
        },
    ];
    let state = crate::tests::wired(AppState::new(meta, blobs, 60, indexes));
    let (status, _, body) = get(&state, "/ov/simple/flask/", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("flask-1.0-py3-none-any.whl"));
}
