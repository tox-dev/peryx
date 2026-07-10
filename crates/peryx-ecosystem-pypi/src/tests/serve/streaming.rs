//! The live page-stream tee and its materialize path.

use super::support::*;

#[tokio::test]
async fn test_stream_detail_offline_cold_miss_falls_back() {
    let dir = tempfile::tempdir().unwrap();
    let state = custom_state(&dir, "https://example.invalid/simple/", |client| {
        vec![Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Cached { client, offline: true },
            policy: peryx_policy::Policy::default(),
        }]
    });

    let outcome = cache::stream_detail(state, 0, "flask".to_owned()).await.unwrap();

    assert!(matches!(outcome, PageOutcome::Fallback));
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
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Cached { client, offline: false },
            policy: peryx_policy::Policy::default(),
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
async fn test_materialize_detail_fetches_and_reuses_cached_page() {
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

    let first = cache::materialize_detail(h.state.clone(), 0, "flask".to_owned())
        .await
        .unwrap()
        .unwrap();
    let second = cache::materialize_detail(h.state.clone(), 0, "flask".to_owned())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(first.name, "flask");
    assert_eq!(first.files, second.files);
    assert!(first.files[0].url.contains(digest.as_str()));
}
#[tokio::test]
async fn test_materialize_detail_returns_stream_errors() {
    let h = harness().await;
    mount_json_page(&h.server, r#"{"name":"flask","files":[{"bad": }]}"#).await;

    let err = cache::materialize_detail(h.state.clone(), 0, "flask".to_owned())
        .await
        .unwrap_err();

    assert!(matches!(err, cache::CacheError::Stream(_)));
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
            ecosystem: peryx_core::Ecosystem::Pypi,
            kind: IndexKind::Cached { client, offline: false },
            policy: peryx_policy::Policy::default(),
        }]
    });
    let items = stream_outcome(&state).await;
    assert!(items.last().is_some_and(Result::is_err));
}
