//! Serving an artifact and coalescing concurrent cold fetches.

use super::support::*;
use peryx_identity::IndexAcl;

#[tokio::test]
async fn test_concurrent_buffered_misses_share_one_fetch() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(150))
                .set_body_raw(
                    detail_json(digest.as_str(), &file_url).into_bytes(),
                    "application/vnd.pypi.simple.v1+json",
                ),
        )
        .expect(1)
        .mount(&h.server)
        .await;
    let (a, b) = tokio::join!(
        get(&h.state, "/pypi/simple/flask/", None),
        get(&h.state, "/pypi/simple/flask/", None),
    );
    assert_eq!((a.0, b.0), (StatusCode::OK, StatusCode::OK));
}
#[tokio::test]
async fn test_concurrent_streaming_misses_share_one_fetch() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(150))
                .set_body_raw(
                    detail_json(digest.as_str(), &file_url).into_bytes(),
                    "application/vnd.pypi.simple.v1+json",
                ),
        )
        .expect(1)
        .mount(&h.server)
        .await;
    let (a, b) = tokio::join!(
        get(&h.state, "/pypi/simple/flask/", Some("application/json")),
        get(&h.state, "/pypi/simple/flask/", Some("application/json")),
    );
    assert_eq!((a.0, b.0), (StatusCode::OK, StatusCode::OK));
    assert!(a.2.contains(digest.as_str()));
    assert!(b.2.contains(digest.as_str()));
}
#[tokio::test]
async fn test_concurrent_buffered_404_waiter_uses_negative_cache() {
    let h = harness().await;
    Mock::given(method("GET"))
        .and(path("/simple/ghost/"))
        .respond_with(ResponseTemplate::new(404).set_delay(std::time::Duration::from_millis(150)))
        .expect(1)
        .mount(&h.server)
        .await;

    let (first, second) = tokio::join!(
        get(&h.state, "/pypi/simple/ghost/", None),
        get(&h.state, "/pypi/simple/ghost/", None),
    );

    assert_eq!((first.0, second.0), (StatusCode::NOT_FOUND, StatusCode::NOT_FOUND));
}
#[tokio::test]
async fn test_concurrent_streaming_404_waiter_uses_negative_cache() {
    let h = harness().await;
    Mock::given(method("GET"))
        .and(path("/simple/ghost/"))
        .respond_with(ResponseTemplate::new(404).set_delay(std::time::Duration::from_millis(150)))
        .expect(1)
        .mount(&h.server)
        .await;

    let (first, second) = tokio::join!(
        get(&h.state, "/pypi/simple/ghost/", Some("application/json")),
        get(&h.state, "/pypi/simple/ghost/", Some("application/json")),
    );

    assert_eq!((first.0, second.0), (StatusCode::NOT_FOUND, StatusCode::NOT_FOUND));
}
#[tokio::test]
async fn test_third_request_hits_the_hot_cache() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            detail_json(digest.as_str(), &file_url).into_bytes(),
            "application/vnd.pypi.simple.v1+json",
        ))
        .expect(1)
        .mount(&h.server)
        .await;
    let first = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    let second = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    let third = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    assert_eq!(first.2, second.2);
    assert_eq!(second.2, third.2);
}
#[tokio::test]
async fn test_file_without_sha256_keeps_its_upstream_url() {
    let h = harness().await;
    let page = r#"{"meta":{"api-version":"1.1"},"name":"flask","versions":["1.0"],
        "files":[{"filename":"flask-1.0.tar.gz","url":"https://up.example/flask-1.0.tar.gz","hashes":{}}]}"#;
    mount_json_page(&h.server, page).await;
    let (status, _, body) = get(&h.state, "/pypi/simple/flask/", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("https://up.example/flask-1.0.tar.gz"));
}
#[tokio::test]
async fn test_file_whose_source_is_not_a_mirror_is_not_found() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    h.state
        .meta
        .put_file_url(digest.as_str(), "https://up.example/x.whl", "hosted")
        .unwrap();
    let uri = format!("/pypi/files/{}/x.whl", digest.as_str());
    let (status, ..) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
#[test]
fn test_cache_error_preserves_simple_api_version_errors() {
    let err = cache::CacheError::from(SimpleError::InvalidApiVersion("1".to_owned()));
    assert!(matches!(err, cache::CacheError::Simple(SimpleError::InvalidApiVersion(version)) if version == "1"));
}
#[test]
fn test_cache_error_user_message_describes_store_and_policy_errors() {
    let meta_err = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
    assert!(
        cache::CacheError::Meta(MetaError::Decode(meta_err))
            .user_message()
            .starts_with("metadata store error:")
    );
    let missing = Digest::of(b"missing");
    assert_eq!(
        cache::CacheError::Blob(BlobError::not_found(&missing)).user_message(),
        format!("blob store error: blob {} not found", missing.as_str())
    );
    assert_eq!(
        cache::CacheError::NotVolatile.user_message(),
        "index is not volatile; delete is disabled"
    );
    assert_eq!(
        cache::CacheError::FileExists("pkg-1.0.whl".to_owned()).user_message(),
        "file \"pkg-1.0.whl\" already exists with different content"
    );
    let config = PolicyConfig {
        block_projects: vec!["flask".to_owned()],
        ..PolicyConfig::default()
    };
    let denial = Policy::compile(&config, crate::normalize_name)
        .check_project(PolicyAction::Serve, "flask")
        .unwrap_err();
    assert_eq!(
        cache::CacheError::Policy(denial).user_message(),
        "project \"flask\" is blocked"
    );
}
#[tokio::test]
async fn test_concurrent_inspect_misses_share_one_fetch() {
    let h = harness().await;
    let wheel = b"not a real archive";
    let digest = Digest::of(wheel);
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_json_page(&h.server, &detail_json(digest.as_str(), &file_url)).await;
    Mock::given(method("GET"))
        .and(path("/files/flask.whl"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(150))
                .set_body_bytes(wheel.to_vec()),
        )
        .expect(1)
        .mount(&h.server)
        .await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    let uri = format!("/pypi/inspect/{}/flask-1.0-py3-none-any.whl", digest.as_str());
    let (first, second) = tokio::join!(get(&h.state, &uri, None), get(&h.state, &uri, None));
    assert_eq!(
        (first.0, second.0),
        (StatusCode::UNPROCESSABLE_ENTITY, StatusCode::UNPROCESSABLE_ENTITY)
    );
    assert!(h.state.blobs.head(&digest).await.unwrap().is_some());
}
#[tokio::test]
async fn test_file_path_returns_blob_cached_while_waiting_for_gate() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let guard = cache::flight_gate(&h.state, digest.as_str()).lock_owned().await;
    let task = tokio::spawn(cache::file_path(
        h.state.serving.clone(),
        digest.clone(),
        "pypi".to_owned(),
        "flask.whl".to_owned(),
    ));
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    h.state.blobs.put_bytes_as(b"wheel", &digest).await.unwrap();
    drop(guard);
    let lease = task.await.unwrap().unwrap();
    assert_eq!(std::fs::read(lease.path()).unwrap(), b"wheel");
}
#[tokio::test]
async fn test_cancelled_download_wakes_waiters_and_leaves_no_entry() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let pending = h.state.blobs.begin().await.unwrap();
    let (mut handle, producer) = h.state.downloads.register(digest.as_str(), pending.tail()).unwrap();

    drop(producer);

    assert!(h.state.downloads.get(digest.as_str()).is_none());
    assert!(matches!(
        handle.progress().borrow_and_update().done.as_ref(),
        Some(Err(message)) if message == "blob transfer abandoned"
    ));
}
#[tokio::test]
async fn test_file_path_offline_mirror_miss_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let state = custom_state(&dir, "https://example.invalid/simple/", |client| {
        vec![Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Cached { client, offline: true },
            policy: peryx_policy::Policy::default(),
            acl: IndexAcl::default(),
        }]
    });
    let digest = Digest::of(b"wheel");
    state
        .meta
        .put_file_url(digest.as_str(), "https://example.invalid/files/flask.whl", "pypi")
        .unwrap();

    let err = cache::file_path(
        state.serving.clone(),
        digest,
        "pypi".to_owned(),
        "flask-1.0-py3-none-any.whl".to_owned(),
    )
    .await
    .unwrap_err();

    assert!(matches!(err, cache::CacheError::OfflineMissing("file")));
}
#[tokio::test]
async fn test_truncated_cached_page_falls_back_and_fails_loudly() {
    let h = harness().await;
    h.state
        .meta
        .put_index("pypi/flask", &fresh_record(br#"{"files":["#))
        .unwrap();
    let (status, ..) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}
#[cfg(unix)]
#[tokio::test]
async fn test_unreadable_cached_blob_is_not_found() {
    use std::os::unix::fs::PermissionsExt as _;
    let h = harness().await;
    let wheel = b"wheelcontent";
    let digest = Digest::of(wheel);
    h.state.blobs.put_bytes_as(wheel, &digest).await.unwrap();
    let lease = h.state.blobs.materialize(&digest).await.unwrap();
    std::fs::set_permissions(lease.path(), std::fs::Permissions::from_mode(0o000)).unwrap();
    let uri = format!("/pypi/files/{}/flask-1.0-py3-none-any.whl", digest.as_str());
    let (status, ..) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[cfg(unix)]
#[tokio::test]
async fn test_download_policy_reports_a_blob_head_error() {
    let active = Policy::compile(
        &PolicyConfig {
            max_file_size_bytes: Some(1024),
            ..PolicyConfig::default()
        },
        crate::normalize_name,
    );
    let h = crate::tests::http::harness_with_policies(true, true, active, Policy::default(), Policy::default()).await;
    let digest = Digest::of(b"loop");
    let hex = digest.as_str();
    let path = h
        .dir
        .path()
        .join(format!("blobs/sha256/{}/{}/{}", &hex[..2], &hex[2..4], hex));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&path, &path).unwrap();

    let uri = format!("/pypi/files/{}/flask-1.0.whl", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body.contains("filesystem blob backend head"), "{body}");
}
