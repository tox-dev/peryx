//! A virtual index serving across its layers.

use super::support::*;

#[tokio::test]
async fn test_overlay_serves_buffered_when_mirror_layer_policy_is_active() {
    let mirror_policy = policy(|neutral, _pypi| {
        neutral.block_projects = vec!["blocked".to_owned()];
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
        neutral.block_projects = vec!["blocked".to_owned()];
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
async fn test_overlay_tolerates_unavailable_layer() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let upstream = UpstreamClient::new("http://127.0.0.1:0/simple/").unwrap();
    let indexes = vec![
        Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Cached {
                client: upstream,
                offline: false,
            },
            policy: Policy::default(),
        },
        Index {
            name: "hosted".to_owned(),
            route: "hosted".to_owned(),
            policy: Policy::default(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Hosted {
                upload_token: Some("s3cret".to_owned()),
                volatile: true,
            },
        },
        Index {
            name: "root/pypi".to_owned(),
            route: "root/pypi".to_owned(),
            policy: Policy::default(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Virtual {
                layers: vec![1, 0],
                upload: Some(1),
            },
        },
    ];
    let state = crate::tests::wired(AppState::new(meta, blobs, 60, indexes));
    upload_peryxpkg(&state, "/root/pypi/", &fixture_wheel()).await;
    // The cached layer is unreachable, but the local layer still serves the upload.
    let (status, _, detail) = get(&state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(detail.contains("peryxpkg"));
}
#[tokio::test]
async fn test_overlay_upload_only_project_unknown_elsewhere() {
    let h = harness().await;
    // Upstream 404s for the project: only the local layer answers, exercising the not-found layer path.
    Mock::given(method("GET"))
        .and(path("/simple/peryxpkg/"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&h.server)
        .await;
    upload_wheel(&h.state, "peryxpkg-1.0-py3-none-any.whl", &fixture_wheel()).await;
    let (status, _, detail) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(detail.contains("peryxpkg-1.0-py3-none-any.whl"));
}
#[tokio::test]
async fn test_overlay_without_upload_layer_serves_merged_page() {
    let dir = tempfile::tempdir().unwrap();
    let server = MockServer::start().await;
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let upstream = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let digest = Digest::of(b"wheel");
    mount_detail(&server, digest.as_str(), "http://x/flask-1.0-py3-none-any.whl", None).await;
    let indexes = vec![
        Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Cached {
                client: upstream,
                offline: false,
            },
            policy: Policy::default(),
        },
        Index {
            name: "ov".to_owned(),
            route: "ov".to_owned(),
            policy: Policy::default(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Virtual {
                layers: vec![0],
                upload: None,
            },
        },
    ];
    let state = crate::tests::wired(AppState::new(meta, blobs, 60, indexes));
    let (status, _, body) = get(&state, "/ov/simple/flask/", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("flask-1.0-py3-none-any.whl"));
}
