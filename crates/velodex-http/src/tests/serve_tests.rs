use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::http::StatusCode;
use bytes::Bytes;
use futures_util::StreamExt as _;
use velodex_core::pypi::SimpleError;
use velodex_storage::blob::{BlobError, BlobStore, Digest};
use velodex_storage::meta::{CachedIndex, MetaError, MetaStore};
use velodex_upstream::UpstreamClient;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::http_tests::{detail_json, get, harness};
use crate::cache::{self, PageOutcome};
use crate::state::{AppState, Index, IndexKind};

fn fresh_record(body: &[u8]) -> CachedIndex {
    CachedIndex {
        etag: None,
        last_serial: None,
        fetched_at_unix: 1000,
        content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
        fresh_secs: None,
        body: body.to_vec(),
    }
}

async fn mount_json_page(server: &MockServer, body: &str) {
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "application/vnd.pypi.simple.v1+json"),
        )
        .mount(server)
        .await;
}

fn split_project_upstream(first: Vec<u8>, rest: Vec<u8>) -> (String, std::sync::mpsc::Sender<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (release, released) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        use std::io::{Read as _, Write as _};
        let (mut socket, _) = listener.accept().unwrap();
        let mut buffer = [0u8; 1024];
        let _ = socket.read(&mut buffer);
        let header = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/vnd.pypi.simple.v1+json\r\ncontent-length: {}\r\n\r\n",
            first.len() + rest.len()
        );
        socket.write_all(header.as_bytes()).unwrap();
        socket.write_all(&first).unwrap();
        socket.flush().unwrap();
        released.recv().unwrap();
        socket.write_all(&rest).unwrap();
    });
    (format!("http://{addr}/simple/"), release)
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
async fn test_negative_cache_expires_by_clock() {
    let h = harness().await;

    h.state.remember_negative("missing".to_owned(), 30);
    assert!(h.state.negative_fresh("missing"));
    h.clock.fetch_add(31, Ordering::Relaxed);

    assert!(!h.state.negative_fresh("missing"));
    assert!(!h.state.negative_fresh("missing"));
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
async fn test_gate_waiter_finds_the_hot_entry_after_a_revalidation() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    let page = ResponseTemplate::new(200).insert_header("etag", "\"v1\"");
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(page.set_body_raw(
            detail_json(digest.as_str(), &file_url).into_bytes(),
            "application/vnd.pypi.simple.v1+json",
        ))
        .mount(&h.server)
        .await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    // Past freshness: both racers revalidate; a 304 refills the hot cache without an epoch bump,
    // so the gate waiter's post-gate hot check hits.
    h.server.reset().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(304).set_delay(std::time::Duration::from_millis(150)))
        .mount(&h.server)
        .await;
    h.clock.fetch_add(61, Ordering::Relaxed);
    let (a, b) = tokio::join!(
        get(&h.state, "/pypi/simple/flask/", Some("application/json")),
        get(&h.state, "/pypi/simple/flask/", Some("application/json")),
    );
    assert_eq!((a.0, b.0), (StatusCode::OK, StatusCode::OK));
    assert_eq!(a.2, b.2);
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
        .put_file_url(digest.as_str(), "https://up.example/x.whl", "local")
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
    assert_eq!(
        cache::CacheError::Blob(BlobError::NotFound("sha256:abc".to_owned())).user_message(),
        "blob store error: blob sha256:abc not found"
    );
    assert_eq!(
        cache::CacheError::NotVolatile.user_message(),
        "index is not volatile; delete is disabled"
    );
    assert_eq!(
        cache::CacheError::FileExists("pkg-1.0.whl".to_owned()).user_message(),
        "file \"pkg-1.0.whl\" already exists with different content"
    );
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
    assert!(h.state.blobs.exists(&digest));
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
        (first.0, second.0, h.state.blobs.exists(&digest)),
        (StatusCode::UNPROCESSABLE_ENTITY, StatusCode::UNPROCESSABLE_ENTITY, true)
    );
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
    assert!(!h.state.blobs.exists(&digest));
}

#[tokio::test]
async fn test_file_path_returns_blob_cached_while_waiting_for_gate() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let guard = cache::flight_gate(&h.state, digest.as_str()).lock_owned().await;
    let task = tokio::spawn(cache::file_path(
        h.state.clone(),
        digest.clone(),
        "pypi".to_owned(),
        "flask.whl".to_owned(),
    ));
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    h.state.blobs.write_verified(b"wheel", &digest).unwrap();
    drop(guard);
    let path = task.await.unwrap().unwrap();
    assert_eq!(path, h.state.blobs.path_for(&digest));
}

#[tokio::test]
async fn test_file_path_abandoned_download_errors() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let (sender, receiver) = tokio::sync::watch::channel(cache::DownloadProgress::default());
    drop(sender);
    h.state.downloads.lock().expect("downloads lock").insert(
        digest.as_str().to_owned(),
        cache::DownloadHandle::new(h.state.blobs.path_for(&digest), receiver),
    );
    let err = cache::file_path(
        h.state.clone(),
        digest.clone(),
        "pypi".to_owned(),
        "flask.whl".to_owned(),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err,
        cache::CacheError::Stream(message) if message == "blob transfer abandoned"
    ));
}

/// A state with the given indexes over a fresh store, for topologies the shared harness lacks.
fn custom_state(dir: &tempfile::TempDir, upstream: &str, indexes: fn(UpstreamClient) -> Vec<Index>) -> Arc<AppState> {
    let meta = MetaStore::open(dir.path().join("velodex.redb")).unwrap();
    let blobs = BlobStore::new(dir.path().join("blobs"));
    let client = UpstreamClient::new(upstream).unwrap();
    Arc::new(AppState::with_clock(
        meta,
        blobs,
        60,
        indexes(client),
        Arc::new(|| 1000),
    ))
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
                kind: IndexKind::Mirror(client.clone()),
            },
            Index {
                name: "b".to_owned(),
                route: "b".to_owned(),
                kind: IndexKind::Mirror(client),
            },
            Index {
                name: "both".to_owned(),
                route: "both".to_owned(),
                kind: IndexKind::Overlay {
                    layers: vec![0, 1],
                    upload: None,
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
                kind: IndexKind::Mirror(client),
            },
            Index {
                name: "inner".to_owned(),
                route: "inner".to_owned(),
                kind: IndexKind::Overlay {
                    layers: vec![0],
                    upload: None,
                },
            },
            Index {
                name: "outer".to_owned(),
                route: "outer".to_owned(),
                kind: IndexKind::Overlay {
                    layers: vec![1],
                    upload: None,
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
                name: "local".to_owned(),
                route: "local".to_owned(),
                kind: IndexKind::Local {
                    upload_token: None,
                    volatile: true,
                },
            },
            Index {
                name: "only".to_owned(),
                route: "only".to_owned(),
                kind: IndexKind::Overlay {
                    layers: vec![0],
                    upload: Some(0),
                },
            },
        ]
    });
    let (status, ..) = get(&state, "/only/simple/ghost/", Some("application/json")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_corrupt_cached_page_falls_back_and_fails_loudly() {
    let h = harness().await;
    h.state
        .meta
        .put_index("pypi/flask", &fresh_record(br#"{"files":[{"bad": }]}"#))
        .unwrap();
    let (status, ..) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
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

async fn stream_outcome(state: &Arc<AppState>) -> Vec<Result<Bytes, std::io::Error>> {
    match cache::stream_detail(state.clone(), 0, "flask".to_owned())
        .await
        .unwrap()
    {
        PageOutcome::Streaming(stream) => stream.collect().await,
        outcome => panic!("expected a streaming outcome, got {}", matches_name(&outcome)),
    }
}

fn matches_name(outcome: &PageOutcome) -> &'static str {
    match outcome {
        PageOutcome::Ready(_) => "Ready",
        PageOutcome::Streaming(_) => "Streaming",
        PageOutcome::NotFound => "NotFound",
        PageOutcome::Fallback => "Fallback",
    }
}

#[tokio::test]
async fn test_small_json_page_without_meta_completes_during_preflight() {
    let h = harness().await;
    mount_json_page(&h.server, r#"{"name":"flask"}"#).await;
    let outcome = cache::stream_detail(h.state.clone(), 0, "flask".to_owned())
        .await
        .unwrap();
    let bytes = match outcome {
        PageOutcome::Ready(bytes) => bytes,
        outcome => panic!("expected a ready outcome, got {}", matches_name(&outcome)),
    };
    assert_eq!(bytes, Bytes::from_static(br#"{"name":"flask"}"#));
    assert!(h.state.meta.get_index("pypi/flask").unwrap().is_some());
}

#[tokio::test]
async fn test_json_meta_preflight_streams_remainder() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    let page = format!(
        "{{\"meta\":{{\"api-version\":\"1.4\"}},\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\"url\":\"{file_url}\",\
         \"hashes\":{{\"sha256\":\"{digest}\"}}}}]}}",
        digest = digest.as_str(),
    );
    mount_json_page(&h.server, &page).await;
    let body = stream_outcome(&h.state)
        .await
        .into_iter()
        .map(Result::unwrap)
        .fold(Vec::new(), |mut body, chunk| {
            body.extend_from_slice(&chunk);
            body
        });
    assert!(String::from_utf8(body).unwrap().contains(digest.as_str()));
}

#[tokio::test]
async fn test_json_meta_preflight_streams_without_remainder() {
    let (upstream, release) = split_project_upstream(br#"{"meta":{"api-version":"1.4"}"#.to_vec(), br"}".to_vec());
    let dir = tempfile::tempdir().unwrap();
    let state = custom_state(&dir, &upstream, |client| {
        vec![Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            kind: IndexKind::Mirror(client),
        }]
    });
    let outcome = cache::stream_detail(state, 0, "flask".to_owned()).await.unwrap();
    release.send(()).unwrap();
    let PageOutcome::Streaming(stream) = outcome else {
        panic!("expected a streaming outcome, got {}", matches_name(&outcome));
    };
    let body = stream
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(Result::unwrap)
        .fold(Vec::new(), |mut body, chunk| {
            body.extend_from_slice(&chunk);
            body
        });
    assert_eq!(String::from_utf8(body).unwrap(), r#"{"meta":{"api-version":"1.4"}}"#);
}

#[tokio::test]
async fn test_live_stream_surfaces_malformed_file_objects() {
    let h = harness().await;
    mount_json_page(&h.server, r#"{"name":"flask","files":[{"bad": }]}"#).await;
    let items = stream_outcome(&h.state).await;
    assert!(items.iter().any(Result::is_err));
}

#[tokio::test]
async fn test_live_stream_surfaces_truncated_pages() {
    let h = harness().await;
    mount_json_page(&h.server, r#"{"name":"flask","files":["#).await;
    let items = stream_outcome(&h.state).await;
    assert!(items.last().is_some_and(Result::is_err));
}

#[tokio::test]
async fn test_live_stream_with_trailing_garbage_errors_and_never_persists() {
    let h = harness().await;
    mount_json_page(&h.server, r#"{"name":"flask","versions":["1.0"],"files":[]}trailing"#).await;
    let items = stream_outcome(&h.state).await;
    // The transformer flags data after the document root, so the stream ends in an error…
    assert!(items.last().is_some_and(Result::is_err));
    // …and the malformed page is never admitted into the cache.
    assert!(h.state.meta.get_index("pypi/flask").unwrap().is_none());
}

#[tokio::test]
async fn test_stats_endpoint_drills_by_index_and_project() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_json_page(&h.server, &detail_json(digest.as_str(), &file_url)).await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    for _ in 0..500 {
        if !h.state.metrics.index_totals().is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let (status, _, body) = get(&h.state, "/+stats", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("pypi"));
    let (status, _, body) = get(&h.state, "/+stats?index=pypi&project=flask", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("files"));
}

#[cfg(unix)]
#[tokio::test]
async fn test_unreadable_cached_blob_is_not_found() {
    use std::os::unix::fs::PermissionsExt as _;
    let h = harness().await;
    let wheel = b"wheelcontent";
    let digest = Digest::of(wheel);
    h.state.blobs.write_verified(wheel, &digest).unwrap();
    let path = h.state.blobs.path_for(&digest);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    let uri = format!("/pypi/files/{}/flask-1.0-py3-none-any.whl", digest.as_str());
    let (status, ..) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
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
async fn test_legacy_cached_record_registers_nothing() {
    let h = harness().await;
    let body = br#"{"meta":{"api-version":"1.1"},"name":"flask","versions":["1.0"],
        "files":[{"filename":"flask-1.0-py3-none-any.whl",
        "url":"/pypi/files/aaaa/flask-1.0-py3-none-any.whl","hashes":{"sha256":"aaaa"}}]}"#;
    cache::persist_page(&h.state, "pypi/flask", "pypi", "flask", &fresh_record(body)).unwrap();
    assert!(h.state.meta.get_file_url("aaaa").unwrap().is_none());
}

#[tokio::test]
async fn test_broken_upstream_transfer_forwards_the_error() {
    let h = harness().await;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        use std::io::{Read as _, Write as _};
        if let Ok((mut socket, _)) = listener.accept() {
            let mut buffer = [0u8; 1024];
            let _ = socket.read(&mut buffer);
            let _ = socket.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\nshort");
        }
    });
    let digest = Digest::of(b"never arrives");
    h.state
        .meta
        .put_file_url(digest.as_str(), &format!("http://{addr}/x.whl"), "pypi")
        .unwrap();
    let outcome = cache::stream_file(h.state.clone(), digest.clone(), "pypi".to_owned(), "x.whl".to_owned())
        .await
        .unwrap();
    let cache::FileOutcome::Live(mut stream) = outcome else {
        panic!("expected a live stream");
    };
    let mut saw_error = false;
    while let Some(item) = stream.next().await {
        saw_error |= item.is_err();
    }
    assert!(saw_error);
    assert!(!h.state.blobs.exists(&digest));
}

#[tokio::test]
async fn test_live_stream_forwards_a_broken_upstream_transfer() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        use std::io::{Read as _, Write as _};
        if let Ok((mut socket, _)) = listener.accept() {
            let mut buffer = [0u8; 1024];
            let _ = socket.read(&mut buffer);
            let _ = socket.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/vnd.pypi.simple.v1+json\r\n\
                  content-length: 500\r\n\r\n{\"name\":\"flask\",\"files\":[",
            );
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let state = custom_state(&dir, &format!("http://{addr}/simple/"), |client| {
        vec![Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            kind: IndexKind::Mirror(client),
        }]
    });
    let items = stream_outcome(&state).await;
    assert!(items.last().is_some_and(Result::is_err));
}

#[tokio::test]
async fn test_buffered_fetch_registers_metadata_siblings() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let meta_digest = Digest::of(b"meta");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    let page = format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\"url\":\"{file_url}\",\
         \"hashes\":{{\"sha256\":\"{digest}\"}},\"core-metadata\":{{\"sha256\":\"{meta}\"}}}}]}}",
        digest = digest.as_str(),
        meta = meta_digest.as_str(),
    );
    mount_json_page(&h.server, &page).await;
    // An HTML request takes the buffered path, whose persistence parses the raw page.
    let (status, ..) = get(&h.state, "/pypi/simple/flask/", None).await;
    assert_eq!(status, StatusCode::OK);
    let (url, meta_sha, _source) = h
        .state
        .meta
        .get_metadata(digest.as_str())
        .unwrap()
        .expect("metadata sibling registered");
    assert_eq!(url, format!("{file_url}.metadata"));
    assert_eq!(meta_sha, meta_digest.as_str());
}
