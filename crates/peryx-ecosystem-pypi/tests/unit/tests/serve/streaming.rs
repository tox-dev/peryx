//! The live page-stream tee and its materialize path.

use super::support::*;
use crate::stream::MAX_PAGE_BYTES;
use peryx_identity::IndexAcl;

/// A `files`-before-`meta` page padded to exactly `len` bytes with insignificant whitespace between
/// the `files` array and `meta`, where JSON allows it. It lets a test place the whole-page buffer on
/// the shared byte cap or one byte past it.
fn padded_files_before_meta_page(file_url: &str, digest: &str, len: usize) -> String {
    let head = format!(
        "{{\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\"url\":\"{file_url}\",\
         \"hashes\":{{\"sha256\":\"{digest}\"}}}}]"
    );
    let tail = r#","meta":{"api-version":"1.4"}}"#;
    let pad = len - head.len() - tail.len();
    format!("{head}{pad}{tail}", pad = " ".repeat(pad))
}

fn files_before_meta_page(file_url: &str, digest: &str, meta: &str) -> String {
    format!(
        "{{\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\"url\":\"{file_url}\",\
         \"hashes\":{{\"sha256\":\"{digest}\"}}}}],\"meta\":{meta}}}"
    )
}

/// A quarantined page whose `versions` array runs past the 64 KiB streaming preflight byte cap before
/// `files`, with `meta` emitted last. A cold stream then leaves preflight with the project status
/// unknown, the exact case where the byte cap used to stream a quarantined project's files.
fn versions_outrun_preflight_page(file_url: &str, digest: &str) -> String {
    use std::fmt::Write as _;
    let mut versions = String::new();
    let mut version = 0u32;
    while versions.len() < 70 * 1024 {
        if version > 0 {
            versions.push(',');
        }
        write!(versions, "\"1.0.{version}\"").unwrap();
        version += 1;
    }
    format!(
        "{{\"name\":\"flask\",\"versions\":[{versions}],\
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\"url\":\"{file_url}\",\
         \"hashes\":{{\"sha256\":\"{digest}\"}}}}],\
         \"meta\":{{\"api-version\":\"1.4\",\"project-status\":\"quarantined\"}}}}"
    )
}

#[tokio::test]
async fn test_stream_detail_offline_cold_miss_falls_back() {
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

    let outcome = cache::stream_detail(state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap();

    assert!(matches!(outcome, PageOutcome::Fallback));
}
#[tokio::test]
async fn test_small_json_page_without_meta_completes_during_preflight() {
    let h = harness().await;
    mount_json_page(&h.server, r#"{"name":"flask"}"#).await;
    let outcome = cache::stream_detail(h.state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap();
    let bytes = match outcome {
        PageOutcome::Ready(bytes, _) => bytes,
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
async fn test_live_stream_records_the_routed_upstream() {
    let server = MockServer::start().await;
    let digest = Digest::of(b"wheel");
    mount_json_page(
        &server,
        &detail_json(digest.as_str(), "https://example.invalid/files/flask.whl"),
    )
    .await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let router = UpstreamRouter::new(vec![NamedUpstream::new("mirror", client.clone())]).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let state = routed_state(&dir, client, router);

    assert!(stream_outcome(&state).await.into_iter().all(|chunk| chunk.is_ok()));
    assert_eq!(
        state
            .meta
            .get_file_url(digest.as_str())
            .unwrap()
            .unwrap()
            .upstream
            .as_deref(),
        Some("mirror")
    );
}

#[tokio::test]
async fn test_json_meta_preflight_streams_without_remainder() {
    let (upstream, release) = split_project_upstream(br#"{"meta":{"api-version":"1.4"}"#.to_vec(), br"}".to_vec());
    let dir = tempfile::tempdir().unwrap();
    let state = custom_state(&dir, &upstream, |client| {
        vec![Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Cached { client, offline: false },
            policy: peryx_policy::Policy::default(),
            acl: IndexAcl::default(),
        }]
    });
    let outcome = cache::stream_detail(state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap();
    release.send(()).unwrap();
    let PageOutcome::Streaming(stream, _) = outcome else {
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

    let first = cache::materialize_detail(h.state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap()
        .unwrap();
    let second = cache::materialize_detail(h.state.serving.clone(), 0, "flask".to_owned())
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
    mount_json_page(
        &h.server,
        r#"{"meta":{"api-version":"1.4"},"name":"flask","files":[{"bad": }]}"#,
    )
    .await;

    let err = cache::materialize_detail(h.state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap_err();

    assert!(matches!(err, cache::CacheError::Stream(_)));
}
#[tokio::test]
async fn test_live_stream_surfaces_malformed_file_objects() {
    let h = harness().await;
    mount_json_page(
        &h.server,
        r#"{"meta":{"api-version":"1.4"},"name":"flask","files":[{"bad": }]}"#,
    )
    .await;
    let items = stream_outcome(&h.state).await;
    assert!(items.iter().any(Result::is_err));
}
#[tokio::test]
async fn test_live_stream_surfaces_truncated_pages() {
    let h = harness().await;
    mount_json_page(&h.server, r#"{"meta":{"api-version":"1.4"},"name":"flask","files":["#).await;
    let items = stream_outcome(&h.state).await;
    assert!(items.last().is_some_and(Result::is_err));
}
#[tokio::test]
async fn test_live_stream_with_trailing_garbage_errors_and_never_persists() {
    let h = harness().await;
    mount_json_page(
        &h.server,
        r#"{"meta":{"api-version":"1.4"},"name":"flask","versions":["1.0"],"files":[]}trailing"#,
    )
    .await;
    let items = stream_outcome(&h.state).await;
    // The transformer flags data after the document root, so the stream ends in an error…
    assert!(items.last().is_some_and(Result::is_err));
    // …and the malformed page is never admitted into the cache.
    assert!(h.state.meta.get_index("pypi/flask").unwrap().is_none());
}
#[tokio::test]
async fn test_live_stream_rejects_malformed_punctuation_and_never_persists() {
    let h = harness().await;
    // A missing value after `"unknown":` balances depth and finishes clean through the structural
    // lexer; the grammar guard fails the body so the malformed page is never cached.
    mount_json_page(
        &h.server,
        r#"{"meta":{"api-version":"1.4"},"name":"flask","versions":["1.0"],"files":[],"unknown":,}"#,
    )
    .await;
    let items = stream_outcome(&h.state).await;
    assert!(items.last().is_some_and(Result::is_err));
    assert!(h.state.meta.get_index("pypi/flask").unwrap().is_none());
}
#[tokio::test]
async fn test_cold_page_with_a_scalar_root_errors_and_never_persists() {
    let h = harness().await;
    // A bare scalar is valid JSON but not a PEP 691 project-detail object; the cold path must fail
    // it before publishing a cache record.
    mount_json_page(&h.server, "123").await;
    let outcome = cache::stream_detail(h.state.serving.clone(), 0, "flask".to_owned()).await;
    assert!(outcome.is_err());
    assert!(h.state.meta.get_index("pypi/flask").unwrap().is_none());
    drop(
        cache::flight_gate(&h.state.serving, "pypi/flask")
            .try_lock_owned()
            .unwrap(),
    );
}
#[tokio::test]
async fn test_live_stream_error_releases_the_inflight_entry() {
    let h = harness().await;
    mount_json_page(
        &h.server,
        r#"{"meta":{"api-version":"1.4"},"name":"flask","files":[{"bad": }]}"#,
    )
    .await;
    let items = stream_outcome(&h.state).await;
    assert!(items.last().is_some_and(Result::is_err));
    drop(
        cache::flight_gate(&h.state.serving, "pypi/flask")
            .try_lock_owned()
            .unwrap(),
    );
}
#[tokio::test]
async fn test_client_disconnect_releases_the_inflight_entry() {
    let (upstream, _release) = split_project_upstream(
        br#"{"meta":{"api-version":"1.4"},"name":"flask","files":["#.to_vec(),
        br"]}".to_vec(),
    );
    let dir = tempfile::tempdir().unwrap();
    let state = custom_state(&dir, &upstream, |client| {
        vec![Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Cached { client, offline: false },
            policy: peryx_policy::Policy::default(),
            acl: IndexAcl::default(),
        }]
    });
    let PageOutcome::Streaming(stream, _) = cache::stream_detail(state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap()
    else {
        panic!("expected a streaming outcome");
    };
    assert!(
        cache::flight_gate(&state.serving, "pypi/flask")
            .try_lock_owned()
            .is_err()
    );
    drop(stream);
    drop(
        cache::flight_gate(&state.serving, "pypi/flask")
            .try_lock_owned()
            .unwrap(),
    );
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
                  content-length: 500\r\n\r\n{\"meta\":{\"api-version\":\"1.4\"},\"name\":\"flask\",\"files\":[",
            );
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let state = custom_state(&dir, &format!("http://{addr}/simple/"), |client| {
        vec![Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Cached { client, offline: false },
            policy: peryx_policy::Policy::default(),
            acl: IndexAcl::default(),
        }]
    });
    let items = stream_outcome(&state).await;
    assert!(items.last().is_some_and(Result::is_err));
}
#[tokio::test]
async fn test_buffered_files_before_meta_surfaces_a_broken_transfer() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    // `files` opens before `meta`, so the page buffers whole; the socket then closes short of the
    // declared length mid-buffer, so the drain must surface the upstream error rather than persist.
    std::thread::spawn(move || {
        use std::io::{Read as _, Write as _};
        if let Ok((mut socket, _)) = listener.accept() {
            let mut buffer = [0u8; 1024];
            let _ = socket.read(&mut buffer);
            let _ = socket.write_all(
                b"HTTP/1.1 200 OK\r\ncontent-type: application/vnd.pypi.simple.v1+json\r\n\
                  content-length: 500\r\n\r\n{\"name\":\"flask\",\"files\":[{\"filename\":\"a\",",
            );
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let state = custom_state(&dir, &format!("http://{addr}/simple/"), |client| {
        vec![Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Cached { client, offline: false },
            policy: peryx_policy::Policy::default(),
            acl: IndexAcl::default(),
        }]
    });
    let outcome = cache::stream_detail(state.serving.clone(), 0, "flask".to_owned()).await;
    assert!(outcome.is_err());
    assert!(state.meta.get_index("pypi/flask").unwrap().is_none());
    drop(
        cache::flight_gate(&state.serving, "pypi/flask")
            .try_lock_owned()
            .unwrap(),
    );
}
#[tokio::test]
async fn test_live_stream_buffers_quarantined_files_before_meta() {
    let h = harness().await;
    let page = files_before_meta_page(
        &format!("{}/files/flask.whl", h.server.uri()),
        Digest::of(b"wheel").as_str(),
        r#"{"api-version":"1.4","project-status":"quarantined"}"#,
    );
    mount_json_page(&h.server, &page).await;

    // `files` precedes `meta`, so the live path buffers the whole page and transforms it once the
    // quarantine status is known, serving a Ready page with no files.
    let outcome = cache::stream_detail(h.state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap();
    let PageOutcome::Ready(bytes, _) = outcome else {
        panic!("expected a ready outcome, got {}", matches_name(&outcome));
    };
    let detail = crate::parse_detail(&bytes).unwrap();
    assert_eq!(detail.meta.status(), crate::ProjectStatus::Quarantined);
    assert!(detail.files.is_empty());
}
#[tokio::test]
async fn test_live_stream_buffers_files_before_meta_exactly_at_the_byte_cap() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let page = padded_files_before_meta_page(
        &format!("{}/files/flask.whl", h.server.uri()),
        digest.as_str(),
        MAX_PAGE_BYTES,
    );
    assert_eq!(page.len(), MAX_PAGE_BYTES);
    mount_json_page(&h.server, &page).await;

    // A `files`-before-`meta` page sitting exactly on the shared cap still buffers, transforms, and
    // persists: the bound rejects only what passes the cap.
    let outcome = cache::stream_detail(h.state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap();
    let PageOutcome::Ready(bytes, _) = outcome else {
        panic!("expected a ready outcome, got {}", matches_name(&outcome));
    };
    let detail = crate::parse_detail(&bytes).unwrap();
    assert_eq!(detail.files.len(), 1);
    assert!(h.state.meta.get_index("pypi/flask").unwrap().is_some());
}
#[tokio::test]
async fn test_live_stream_rejects_files_before_meta_past_the_byte_cap() {
    let h = harness().await;
    let page = padded_files_before_meta_page(
        &format!("{}/files/flask.whl", h.server.uri()),
        Digest::of(b"wheel").as_str(),
        MAX_PAGE_BYTES + 1024,
    );
    mount_json_page(&h.server, &page).await;

    // The upstream chunks a `files`-before-`meta` body that crosses the cap after preflight. The
    // whole-page buffer stops at the limit instead of holding the oversized body, fails, and leaves
    // the page unpersisted with the flight released.
    let outcome = cache::stream_detail(h.state.serving.clone(), 0, "flask".to_owned()).await;
    assert!(matches!(outcome, Err(cache::CacheError::Unavailable)));
    assert!(h.state.meta.get_index("pypi/flask").unwrap().is_none());
    drop(
        cache::flight_gate(&h.state.serving, "pypi/flask")
            .try_lock_owned()
            .unwrap(),
    );
}
#[tokio::test]
async fn test_live_stream_withholds_quarantined_files_when_versions_outrun_the_preflight_cap() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let page = versions_outrun_preflight_page(&format!("{}/files/flask.whl", h.server.uri()), digest.as_str());
    mount_json_page(&h.server, &page).await;

    // The versions array runs past the 64 KiB preflight byte cap before `files`, so the cold stream
    // reaches preflight with neither `meta` nor `files` seen. The whole page is then buffered and
    // served Ready with the quarantined files withheld; without the fix it streams and leaks them.
    let outcome = cache::stream_detail(h.state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap();
    let PageOutcome::Ready(bytes, _) = outcome else {
        panic!("expected a ready outcome, got {}", matches_name(&outcome));
    };
    let detail = crate::parse_detail(&bytes).unwrap();

    assert_eq!(detail.meta.status(), crate::ProjectStatus::Quarantined);
    assert!(detail.files.is_empty());
}
#[tokio::test]
async fn test_buffered_files_before_meta_surfaces_parse_errors() {
    let h = harness().await;
    // `files` precedes `meta`, so the page is buffered whole; a malformed file then fails the
    // buffered re-parse and releases the flight rather than streaming a half page.
    mount_json_page(&h.server, r#"{"name":"flask","files":[{"bad": }]}"#).await;
    let outcome = cache::stream_detail(h.state.serving.clone(), 0, "flask".to_owned()).await;
    assert!(matches!(outcome, Err(cache::CacheError::Simple(_))));
    drop(
        cache::flight_gate(&h.state.serving, "pypi/flask")
            .try_lock_owned()
            .unwrap(),
    );
}
#[tokio::test]
async fn test_live_stream_buffers_downloadable_files_before_meta() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let page = files_before_meta_page(
        &format!("{}/files/flask.whl", h.server.uri()),
        digest.as_str(),
        r#"{"api-version":"1.4"}"#,
    );
    mount_json_page(&h.server, &page).await;

    // A non-quarantined `files`-before-`meta` page still serves its files, with peryx URLs.
    let outcome = cache::stream_detail(h.state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap();
    let PageOutcome::Ready(bytes, _) = outcome else {
        panic!("expected a ready outcome, got {}", matches_name(&outcome));
    };
    let detail = crate::parse_detail(&bytes).unwrap();
    assert_eq!(detail.files.len(), 1);
    assert!(detail.files[0].url.contains(digest.as_str()));
}
#[tokio::test]
async fn test_transform_whole_withholds_quarantined_files_before_meta() {
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
    let page = files_before_meta_page(
        "https://example.invalid/files/flask.whl",
        Digest::of(b"wheel").as_str(),
        r#"{"api-version":"1.4","project-status":"quarantined"}"#,
    );
    state
        .meta
        .put_index("pypi/flask", &fresh_record(page.as_bytes()))
        .unwrap();

    let outcome = cache::stream_detail(state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap();
    let PageOutcome::Ready(bytes, _) = outcome else {
        panic!("expected a ready outcome, got {}", matches_name(&outcome));
    };
    let detail = crate::parse_detail(&bytes).unwrap();
    assert_eq!(detail.meta.status(), crate::ProjectStatus::Quarantined);
    assert!(detail.files.is_empty());
}
