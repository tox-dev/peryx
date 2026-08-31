use super::support::*;
use crate::stream::MAX_PAGE_BYTES;
use futures_util::TryStreamExt as _;
use peryx_identity::IndexAcl;
use rstest::rstest;

fn padded_files_without_status_page(file_url: &str, digest: &str, len: usize) -> String {
    let head = format!(
        "{{\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\"size\":11,\"url\":\"{file_url}\",\
         \"hashes\":{{\"sha256\":\"{digest}\"}}}}]"
    );
    let tail = r#","meta":{"api-version":"1.4"}}"#;
    let pad = len - head.len() - tail.len();
    format!("{head}{pad}{tail}", pad = " ".repeat(pad))
}

fn files_before_status_page(file_url: &str, digest: &str, status: Option<&str>) -> String {
    let project_status = status.map_or_else(String::new, |status| {
        format!(r#","project-status":{{"status":"{status}"}}"#)
    });
    format!(
        "{{\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\"size\":11,\"url\":\"{file_url}\",\
         \"hashes\":{{\"sha256\":\"{digest}\"}}}}],\"meta\":{{\"api-version\":\"1.4\"}}{project_status}}}"
    )
}

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
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\"size\":11,\"url\":\"{file_url}\",\
         \"hashes\":{{\"sha256\":\"{digest}\"}}}}],\
         \"meta\":{{\"api-version\":\"1.4\"}},\
         \"project-status\":{{\"status\":\"quarantined\"}}}}"
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

    assert!(matches!(streaming_parts(outcome), Err(PageOutcome::Fallback)));
}
#[tokio::test]
async fn test_small_json_page_without_meta_completes_during_preflight() {
    let h = harness().await;
    mount_json_page(&h.server, r#"{"name":"flask"}"#).await;
    let outcome = cache::stream_detail(h.state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        PageOutcome::Ready(bytes, _) if bytes == Bytes::from_static(br#"{"name":"flask"}"#)
    ));
    assert!(h.state.serving.meta.get_index("pypi/flask").unwrap().is_some());
}
#[tokio::test]
async fn test_json_status_preflight_streams_remainder() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    let page = format!(
        "{{\"meta\":{{\"api-version\":\"1.4\"}},\"project-status\":{{}},\
         \"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\"size\":11,\"url\":\"{file_url}\",\
         \"hashes\":{{\"sha256\":\"{digest}\"}}}}]}}",
        digest = digest.as_str(),
    );
    mount_json_page(&h.server, &page).await;
    let outcome = cache::stream_detail(h.state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap();
    let body = streaming_parts(outcome)
        .ok()
        .unwrap()
        .0
        .try_collect::<Vec<_>>()
        .await
        .unwrap()
        .concat();
    let expected = concat!(
            r#"{"meta":{"api-version":"1.4"},"project-status":{},"name":"flask","versions":["1.0"],"files":[{"filename":"flask-1.0-py3-none-any.whl","url":"/pypi/files/"#,
            r#"DIGEST/flask-1.0-py3-none-any.whl","hashes":{"sha256":"DIGEST"},"size":11,"yanked":false,"core-metadata":false}]}"#,
        )
        .replace("DIGEST", digest.as_str());
    assert_eq!(body, expected.as_bytes());
}

#[tokio::test]
async fn test_live_stream_records_the_routed_upstream() {
    let server = MockServer::start().await;
    let digest = Digest::of(b"wheel");
    let page = detail_json(digest.as_str(), "https://example.invalid/files/flask.whl").replacen(
        ",\"name\":",
        ",\"project-status\":{},\"name\":",
        1,
    );
    mount_json_page(&server, &page).await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    let router = UpstreamRouter::new(vec![NamedUpstream::new("mirror", client.clone())]).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let state = routed_state(&dir, client, router);

    let outcome = cache::stream_detail(state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap();
    let body = streaming_parts(outcome)
        .ok()
        .unwrap()
        .0
        .try_collect::<Vec<_>>()
        .await
        .unwrap()
        .concat();
    assert!(!body.is_empty());
    assert_eq!(
        state
            .serving
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
async fn test_json_status_preflight_streams_without_remainder() {
    let upstream = split_project_upstream(
        br#"{"meta":{"api-version":"1.4"},"versions":[],"project-status":{}"#.to_vec(),
        br"}".to_vec(),
    );
    let dir = tempfile::tempdir().unwrap();
    let state = custom_state(&dir, &upstream.upstream, |client| {
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
    upstream.release();
    let body = streaming_parts(outcome)
        .ok()
        .unwrap()
        .0
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(Result::unwrap)
        .fold(Vec::new(), |mut body, chunk| {
            body.extend_from_slice(&chunk);
            body
        });
    assert_eq!(
        body,
        br#"{"meta":{"api-version":"1.4"},"versions":[],"project-status":{}}"#
    );
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
        r#"{"meta":{"api-version":"1.4"},"versions":[],"project-status":{},"name":"flask","files":[{"bad": }]}"#,
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
        r#"{"meta":{"api-version":"1.4"},"versions":[],"project-status":{},"name":"flask","files":[{"bad": }]}"#,
    )
    .await;
    let outcome = cache::stream_detail(h.state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap();
    let items = streaming_parts(outcome).ok().unwrap().0.collect::<Vec<_>>().await;
    assert!(items.iter().any(Result::is_err));
}
#[tokio::test]
async fn test_live_stream_surfaces_truncated_pages() {
    let h = harness().await;
    mount_json_page(
        &h.server,
        r#"{"meta":{"api-version":"1.4"},"versions":[],"project-status":{},"name":"flask","files":["#,
    )
    .await;
    let outcome = cache::stream_detail(h.state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap();
    let items = streaming_parts(outcome).ok().unwrap().0.collect::<Vec<_>>().await;
    assert!(items.last().is_some_and(Result::is_err));
}
#[rstest]
#[case(r#"{"meta":{"api-version":"1.4"},"project-status":{},"name":"flask","versions":["1.0"],"files":[]}trailing"#)]
#[case(
    r#"{"meta":{"api-version":"1.4"},"project-status":{},"name":"flask","versions":["1.0"],"files":[],"unknown":,}"#
)]
#[case(
    "{\"meta\":{\"api-version\":\"1.4\"},\"versions\":[],\"project-status\":{},\"name\":\"flask\"\u{000b},\"files\":[]}"
)]
#[case(
    "{\"meta\":{\"api-version\":\"1.4\"},\"versions\":[],\"project-status\":{},\"name\":\"flask\"\u{000c},\"files\":[]}"
)]
#[tokio::test]
async fn test_live_stream_invalid_document_errors_and_never_persists(#[case] page: &str) {
    let h = harness().await;
    mount_json_page(&h.server, page).await;
    let outcome = cache::stream_detail(h.state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap();
    let items = streaming_parts(outcome).ok().unwrap().0.collect::<Vec<_>>().await;
    assert!(items.last().is_some_and(Result::is_err));
    assert!(h.state.serving.meta.get_index("pypi/flask").unwrap().is_none());
}
#[tokio::test]
async fn test_cold_page_with_a_scalar_root_errors_and_never_persists() {
    let h = harness().await;
    mount_json_page(&h.server, "123").await;
    let outcome = cache::stream_detail(h.state.serving.clone(), 0, "flask".to_owned()).await;
    assert!(outcome.is_err());
    assert!(h.state.serving.meta.get_index("pypi/flask").unwrap().is_none());
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
        r#"{"meta":{"api-version":"1.4"},"versions":[],"project-status":{},"name":"flask","files":[{"bad": }]}"#,
    )
    .await;
    let outcome = cache::stream_detail(h.state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap();
    let items = streaming_parts(outcome).ok().unwrap().0.collect::<Vec<_>>().await;
    assert!(items.last().is_some_and(Result::is_err));
    drop(
        cache::flight_gate(&h.state.serving, "pypi/flask")
            .try_lock_owned()
            .unwrap(),
    );
}
#[tokio::test]
async fn test_client_disconnect_releases_the_inflight_entry() {
    let upstream = split_project_upstream(
        br#"{"meta":{"api-version":"1.4"},"versions":[],"project-status":{},"name":"flask","files":["#.to_vec(),
        br"]}".to_vec(),
    );
    let dir = tempfile::tempdir().unwrap();
    let state = custom_state(&dir, &upstream.upstream, |client| {
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
    let (stream, _) = streaming_parts(outcome).ok().unwrap();
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
    let server = response_server(
        b"HTTP/1.1 200 OK\r\ncontent-type: application/vnd.pypi.simple.v1+json\r\n\
          content-length: 500\r\n\r\n{\"meta\":{\"api-version\":\"1.4\"},\"versions\":[],\"project-status\":{},\"name\":\"flask\",\"files\":[",
    );
    let dir = tempfile::tempdir().unwrap();
    let state = custom_state(&dir, &server.upstream, |client| {
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
    let items = streaming_parts(outcome).ok().unwrap().0.collect::<Vec<_>>().await;
    assert!(items.last().is_some_and(Result::is_err));
}
#[tokio::test]
async fn test_buffered_files_before_status_surfaces_a_broken_transfer() {
    let server = response_server(
        b"HTTP/1.1 200 OK\r\ncontent-type: application/vnd.pypi.simple.v1+json\r\n\
          content-length: 500\r\n\r\n{\"name\":\"flask\",\"files\":[{\"filename\":\"a\",",
    );
    let dir = tempfile::tempdir().unwrap();
    let state = custom_state(&dir, &server.upstream, |client| {
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
    assert!(state.serving.meta.get_index("pypi/flask").unwrap().is_none());
    drop(
        cache::flight_gate(&state.serving, "pypi/flask")
            .try_lock_owned()
            .unwrap(),
    );
}
#[tokio::test]
async fn test_live_stream_buffers_quarantined_files_before_status() {
    let h = harness().await;
    let page = files_before_status_page(
        &format!("{}/files/flask.whl", h.server.uri()),
        Digest::of(b"wheel").as_str(),
        Some("quarantined"),
    );
    mount_json_page(&h.server, &page).await;

    let outcome = cache::stream_detail(h.state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        PageOutcome::Ready(bytes, _) if crate::parse_detail(&bytes).is_ok_and(|detail| (
            detail.meta.status(),
            detail.files.is_empty(),
        ) == (crate::ProjectStatus::Quarantined, true))
    ));
}
#[tokio::test]
async fn test_live_stream_buffers_files_without_status_exactly_at_the_byte_cap() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let page = padded_files_without_status_page(
        &format!("{}/files/flask.whl", h.server.uri()),
        digest.as_str(),
        MAX_PAGE_BYTES,
    );
    assert_eq!(page.len(), MAX_PAGE_BYTES);
    mount_json_page(&h.server, &page).await;

    // The byte cap is inclusive.
    let outcome = cache::stream_detail(h.state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        PageOutcome::Ready(bytes, _) if crate::parse_detail(&bytes).is_ok_and(|detail| detail.files.len() == 1)
    ));
    assert!(h.state.serving.meta.get_index("pypi/flask").unwrap().is_some());
}
#[tokio::test]
async fn test_live_stream_rejects_files_without_status_past_the_byte_cap() {
    let h = harness().await;
    let page = padded_files_without_status_page(
        &format!("{}/files/flask.whl", h.server.uri()),
        Digest::of(b"wheel").as_str(),
        MAX_PAGE_BYTES + 1024,
    );
    mount_json_page(&h.server, &page).await;

    // The post-preflight buffer must enforce the same byte cap.
    let outcome = cache::stream_detail(h.state.serving.clone(), 0, "flask".to_owned()).await;
    assert!(matches!(outcome, Err(cache::CacheError::Unavailable)));
    assert!(h.state.serving.meta.get_index("pypi/flask").unwrap().is_none());
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

    // Preflight exhaustion must not leak quarantined files before `project-status` arrives.
    let outcome = cache::stream_detail(h.state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        PageOutcome::Ready(bytes, _) if crate::parse_detail(&bytes).is_ok_and(|detail| (
            detail.meta.status(),
            detail.files.is_empty(),
        ) == (crate::ProjectStatus::Quarantined, true))
    ));
}
#[tokio::test]
async fn test_buffered_files_before_status_surfaces_parse_errors() {
    let h = harness().await;
    // Buffered parse failures must release the shared flight.
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
async fn test_live_stream_buffers_downloadable_files_without_status() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let page = files_before_status_page(&format!("{}/files/flask.whl", h.server.uri()), digest.as_str(), None);
    mount_json_page(&h.server, &page).await;

    let outcome = cache::stream_detail(h.state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        PageOutcome::Ready(bytes, _) if crate::parse_detail(&bytes).is_ok_and(|detail| detail.files.len() == 1
            && detail.files[0].url.contains(digest.as_str()))
    ));
}
#[tokio::test]
async fn test_transform_whole_withholds_quarantined_files_before_status() {
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
    let page = files_before_status_page(
        "https://example.invalid/files/flask.whl",
        Digest::of(b"wheel").as_str(),
        Some("quarantined"),
    );
    state
        .serving
        .meta
        .put_index("pypi/flask", &fresh_record(page.as_bytes()))
        .unwrap();

    let outcome = cache::stream_detail(state.serving.clone(), 0, "flask".to_owned())
        .await
        .unwrap();
    assert!(matches!(
        outcome,
        PageOutcome::Ready(bytes, _) if crate::parse_detail(&bytes).is_ok_and(|detail| (
            detail.meta.status(),
            detail.files.is_empty(),
        ) == (crate::ProjectStatus::Quarantined, true))
    ));
}
