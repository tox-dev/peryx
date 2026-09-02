use super::support::*;
use peryx_identity::IndexAcl;

#[tokio::test]
async fn test_file_download_fetches_verifies_and_caches() {
    let h = harness().await;
    let wheel = b"wheelcontent";
    let digest = Digest::of(wheel);
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_detail(&h.server, digest.as_str(), &file_url, None).await;
    Mock::given(method("GET"))
        .and(path("/files/flask.whl"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(wheel.to_vec()))
        .expect(1)
        .mount(&h.server)
        .await;

    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    let uri = format!("/pypi/files/{}/flask-1.0-py3-none-any.whl", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "wheelcontent");
    let (status2, _, body2) = get(&h.state, &uri, None).await;
    assert_eq!(status2, StatusCode::OK);
    assert_eq!(body2, body);
}

#[rstest]
#[case::valid(b"expected wheel", true)]
#[case::digest_mismatch(b"wrong bytes", false)]
#[tokio::test]
async fn test_routed_file_download_verifies_the_advertising_source(#[case] artifact: &[u8], #[case] valid: bool) {
    let logs = LogCapture::default();
    let _guard = logs.install();
    let first = MockServer::start().await;
    let second = MockServer::start().await;
    let artifact_server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&first)
        .await;
    let wheel = b"expected wheel";
    let digest = Digest::of(wheel);
    let file_url = format!("{}/files/flask.whl", second.uri());
    mount_detail(&second, digest.as_str(), &file_url, None).await;
    Mock::given(method("GET"))
        .and(path("/files/flask.whl"))
        .and(match_header("authorization", "Bearer second-token"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(artifact.to_vec()))
        .expect(1)
        .mount(&artifact_server)
        .await;
    let primary = UpstreamClient::new(&format!("{}/simple/", first.uri())).unwrap();
    let upstream_router = UpstreamRouter::new(vec![
        NamedUpstream::new("first", primary.clone()),
        NamedUpstream::new(
            "second",
            UpstreamClient::with_auth(
                &format!("{}/simple/", second.uri()),
                Auth::Bearer("second-token".to_owned()),
            )
            .unwrap(),
        )
        .with_artifact_mirror(
            UpstreamClient::with_auth(&artifact_server.uri(), Auth::Bearer("second-token".to_owned())).unwrap(),
            true,
        ),
    ])
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let state = routed_state(&dir, primary, upstream_router);

    get(&state, "/pypi/simple/flask/", Some("application/json")).await;
    let uri = format!("/pypi/files/{}/flask-1.0-py3-none-any.whl", digest.as_str());
    let response = router(state.clone())
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = tokio::time::timeout(std::time::Duration::from_secs(2), response.into_body().collect())
        .await
        .expect("download body completes");
    state.serving.metrics.flush().unwrap();
    if valid {
        assert_eq!(body.unwrap().to_bytes(), wheel.as_slice());
        assert!(state.serving.blobs.head(&digest).await.unwrap().is_some());
        let usage = state.serving.metrics.daily_usage();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].group, "1.0");
        assert_eq!(usage[0].source, "second");
        assert_eq!(usage[0].reads, 1);
    } else {
        assert!(body.is_err());
        assert!(state.serving.blobs.head(&digest).await.unwrap().is_none());
        assert!(state.serving.metrics.index_totals()["pypi"].base.rejected >= 1);
    }
    artifact_server.verify().await;
    let log_text = logs.text();
    let event = log_text
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|event| field(event, "message") == Some("blob transfer ended"))
        .expect(&log_text);
    assert_eq!(field(&event, "upstream"), Some("second"));
}

#[rstest]
#[case::fallback(true, StatusCode::OK, 1)]
#[case::no_fallback(false, StatusCode::BAD_GATEWAY, 0)]
#[tokio::test]
async fn test_artifact_mirror_honors_repository_fallback(
    #[case] fallback: bool,
    #[case] expected_status: StatusCode,
    #[case] origin_requests: u64,
) {
    let origin = MockServer::start().await;
    let mirror = MockServer::start().await;
    let wheel = b"wheelcontent";
    let digest = Digest::of(wheel);
    let file_url = format!("{}/files/flask.whl?origin=1", origin.uri());
    // PEP 658 prevents metadata backfill from racing request-count assertions.
    let metadata_digest = Digest::of(b"flask metadata");
    let detail = format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\"size\":11,\"url\":\"{file_url}\",\
         \"hashes\":{{\"sha256\":\"{}\"}},\"core-metadata\":{{\"sha256\":\"{}\"}}}}]}}",
        digest.as_str(),
        metadata_digest.as_str()
    );
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(detail.into_bytes(), "application/vnd.pypi.simple.v1+json"),
        )
        .mount(&origin)
        .await;
    Mock::given(method("GET"))
        .and(path("/packages/files/flask.whl"))
        .respond_with(ResponseTemplate::new(404))
        .expect(1)
        .mount(&mirror)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/flask.whl"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(wheel.to_vec()))
        .expect(origin_requests)
        .mount(&origin)
        .await;
    let client = UpstreamClient::new(&format!("{}/simple/", origin.uri())).unwrap();
    let upstream = NamedUpstream::new("origin", client.clone()).with_artifact_mirror(
        UpstreamClient::new(&format!("{}/packages/", mirror.uri())).unwrap(),
        fallback,
    );
    let upstream_router = UpstreamRouter::new(vec![upstream]).unwrap().with_fallback(fallback);
    let dir = tempfile::tempdir().unwrap();
    let state = routed_state(&dir, client, upstream_router);

    get(&state, "/pypi/simple/flask/", Some("application/json")).await;
    let uri = format!("/pypi/files/{}/flask-1.0-py3-none-any.whl", digest.as_str());
    let (status, _, body) = get(&state, &uri, None).await;

    assert_eq!(status, expected_status);
    if fallback {
        assert_eq!(body, "wheelcontent");
        assert!(state.serving.blobs.head(&digest).await.unwrap().is_some());
    } else {
        assert!(body.contains("upstream returned 404 Not Found"));
        assert!(state.serving.blobs.head(&digest).await.unwrap().is_none());
    }
}
#[tokio::test]
async fn test_quarantined_project_hides_files_and_blocks_downloads() {
    let h = harness().await;
    let wheel = b"wheelcontent";
    let digest = Digest::of(wheel);
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_detail(&h.server, digest.as_str(), &file_url, Some("\"active\"")).await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    h.server.reset().await;
    mount_status_detail(&h.server, "flask", "quarantined", "malware", digest.as_str(), &file_url).await;
    h.clock.store(5000, Ordering::Relaxed);

    let (status, _, detail) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["project-status"]["status"], "quarantined");
    assert!(detail["files"].as_array().unwrap().is_empty());

    let uri = format!("/pypi/files/{}/flask-1.0-py3-none-any.whl", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        body,
        "project for file \"flask-1.0-py3-none-any.whl\" is quarantined; downloads are disabled"
    );

    let overlay_uri = format!("/root/pypi/files/{}/flask-1.0-py3-none-any.whl", digest.as_str());
    let (status, _, body) = get(&h.state, &overlay_uri, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        body,
        "project for file \"flask-1.0-py3-none-any.whl\" is quarantined; downloads are disabled"
    );
}
#[tokio::test]
async fn test_file_download_status_store_error_is_server_error() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("peryx.redb");
    MetaStore::open(&db_path).unwrap();
    put_raw_project_status(&db_path, "pypi/flask", b"not json");
    let meta = MetaStore::open(&db_path).unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let upstream = UpstreamClient::new("http://127.0.0.1:0/simple/").unwrap();
    let indexes = vec![Index {
        name: "pypi".to_owned(),
        route: "pypi".to_owned(),
        ecosystem: crate::ECOSYSTEM,
        kind: IndexKind::Cached {
            client: upstream,
            offline: false,
        },
        policy: Policy::default(),
        acl: IndexAcl::default(),
    }];
    let state = crate::tests::wired(AppState::new(meta, blobs, 60, indexes));

    let uri = format!(
        "/pypi/files/{}/flask-1.0-py3-none-any.whl",
        Digest::of(b"wheel").as_str()
    );
    let (status, _, body) = get(&state, &uri, None).await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body.contains("file download on index \"pypi\""));
    assert!(body.contains("metadata store error"));
}
#[tokio::test]
async fn test_file_download_invalid_digest_is_bad_request() {
    let h = harness().await;
    let (status, _, body) = get(&h.state, "/pypi/files/notahex/x.whl", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("expected 64 lowercase hex sha256"));
}
#[tokio::test]
async fn test_file_download_rejects_encoded_path_filename() {
    let h = harness().await;
    let uri = format!("/pypi/files/{}/pkg%2Fname.whl", "a".repeat(64));
    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body,
        "invalid artifact name \"pkg/name.whl\": artifact names must be relative path segments without separators, traversal, or control characters"
    );
}
#[tokio::test]
async fn test_file_download_allows_literal_percent_filename() {
    let h = harness().await;
    let digest = put_local_file(&h.state, "peryxpkg-1.0%2F.tar.gz", b"PKpercent", "1.0");
    let uri = format!("/hosted/files/{}/peryxpkg-1.0%252F.tar.gz", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "PKpercent");
}
#[tokio::test]
async fn test_file_download_unknown_digest_is_not_found() {
    let h = harness().await;
    let uri = format!("/pypi/files/{}/x.whl", "a".repeat(64));
    let (status, ..) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
#[tokio::test]
async fn test_file_source_not_a_mirror_is_not_found() {
    let h = harness().await;
    let digest = Digest::of(b"orphan");
    crate::tests::register_publication(&h.state.serving.meta, "pypi", "orphan.whl", digest.as_str(), None);
    h.state
        .serving
        .meta
        .put_file_url(digest.as_str(), "http://x/orphan.whl", "hosted")
        .unwrap();
    let uri = format!("/pypi/files/{}/orphan.whl", digest.as_str());
    let (status, ..) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
#[tokio::test]
async fn test_file_digest_mismatch_fails_the_body_and_never_persists() {
    let h = harness().await;
    let digest = Digest::of(b"expected");
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_detail(&h.server, digest.as_str(), &file_url, None).await;
    Mock::given(method("GET"))
        .and(path("/files/flask.whl"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"wrong bytes".to_vec()))
        .mount(&h.server)
        .await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    let uri = format!("/pypi/files/{}/flask-1.0-py3-none-any.whl", digest.as_str());
    let response = router(h.state.clone())
        .oneshot(Request::builder().uri(&*uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(2), response.into_body().collect(),)
            .await
            .expect("download body completes")
            .is_err()
    );
    h.state.serving.metrics.flush().unwrap();
    assert!(h.state.serving.blobs.head(&digest).await.unwrap().is_none());
    assert_eq!(h.state.serving.metrics.index_totals()["pypi"].base.rejected, 1);
}
const WHEEL: &[u8] = b"wheelcontent";

/// A wheel already in the blob store and published by `pypi`, which is what a page fetch leaves
/// behind and what the file route now requires before it releases the bytes.
fn cached_wheel_uri(h: &Harness) -> String {
    let digest = Digest::of(WHEEL);
    h.state.serving.blobs.blocking().put_bytes_as(WHEEL, &digest).unwrap();
    crate::tests::register_publication(
        &h.state.serving.meta,
        "pypi",
        "flask-1.0-py3-none-any.whl",
        digest.as_str(),
        None,
    );
    format!("/pypi/files/{}/flask-1.0-py3-none-any.whl", digest.as_str())
}

#[tokio::test]
async fn test_cached_file_without_a_range_serves_the_whole_wheel() {
    let h = harness().await;
    let uri = cached_wheel_uri(&h);

    let (status, headers, body) = get_bytes(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::ACCEPT_RANGES], "bytes");
    assert_eq!(headers[header::CONTENT_LENGTH], WHEEL.len().to_string());
    assert!(!headers.contains_key(header::CONTENT_RANGE));
    assert_eq!(body, WHEEL);
}
#[rstest]
#[case::bounded("bytes=2-5", 2, 5)]
#[case::open_ended("BYTES=6-", 6, 11)]
#[case::suffix("Bytes=-4", 8, 11)]
#[case::suffix_past_the_start("bytes=-99", 0, 11)]
#[case::end_past_the_last_byte("bytes=8-99", 8, 11)]
#[tokio::test]
async fn test_cached_file_serves_a_byte_range(#[case] range: &str, #[case] start: usize, #[case] end: usize) {
    let h = harness().await;
    let uri = cached_wheel_uri(&h);

    let (status, headers, body) = get_bytes_with_headers(&h.state, &uri, &[("range", range)]).await;

    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        headers[header::CONTENT_RANGE],
        format!("bytes {start}-{end}/{}", WHEEL.len())
    );
    assert_eq!(headers[header::CONTENT_LENGTH], (end - start + 1).to_string());
    assert_eq!(headers[header::ACCEPT_RANGES], "bytes");
    assert_eq!(body, WHEEL[start..=end]);
}
#[rstest]
#[case::start_past_the_last_byte("bytes=12-")]
#[case::wholly_out_of_bounds("bytes=99-100")]
#[case::empty_suffix("bytes=-0")]
#[tokio::test]
async fn test_cached_file_refuses_an_unsatisfiable_range(#[case] range: &str) {
    let h = harness().await;
    let uri = cached_wheel_uri(&h);

    let (status, headers, body) = get_bytes_with_headers(&h.state, &uri, &[("range", range)]).await;

    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(headers[header::CONTENT_RANGE], format!("bytes */{}", WHEEL.len()));
    assert_eq!(headers[header::ACCEPT_RANGES], "bytes");
    assert!(body.is_empty());
}
#[rstest]
#[case::malformed("bytes=abc-")]
#[case::backwards("bytes=5-2")]
#[case::unsupported_unit("items=0-1")]
#[case::multiple("bytes=0-1,4-5")]
#[tokio::test]
async fn test_cached_file_serves_the_whole_wheel_for_a_range_it_cannot_read(#[case] range: &str) {
    let h = harness().await;
    let uri = cached_wheel_uri(&h);

    let (status, headers, body) = get_bytes_with_headers(&h.state, &uri, &[("range", range)]).await;

    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key(header::CONTENT_RANGE));
    assert_eq!(body, WHEEL);
}
fn wheel_etag() -> String {
    format!("\"{}\"", Digest::of(WHEEL).as_str())
}

#[rstest]
#[case::get("GET")]
#[case::head("HEAD")]
#[tokio::test]
async fn test_cached_file_is_served_under_its_digest_as_an_entity_tag(#[case] verb: &str) {
    let h = harness().await;
    let uri = cached_wheel_uri(&h);

    let (status, headers, _) = send_bytes(&h.state, verb, &uri, &[]).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::ETAG], wheel_etag());
}
#[rstest]
#[case::exact(&wheel_etag())]
#[case::weak(&format!("W/{}", wheel_etag()))]
#[case::any("*")]
#[case::list(&format!("\"0000\", {}", wheel_etag()))]
#[tokio::test]
async fn test_cached_file_matching_if_none_match_is_not_modified(#[case] field: &str) {
    let h = harness().await;
    let uri = cached_wheel_uri(&h);

    let (status, headers, body) = get_bytes_with_headers(&h.state, &uri, &[("if-none-match", field)]).await;

    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert_eq!(headers[header::ETAG], wheel_etag());
    assert!(body.is_empty());
}
#[rstest]
#[case::other_digest("\"0000\"")]
#[case::malformed("not-a-tag")]
#[tokio::test]
async fn test_cached_file_serves_the_wheel_for_an_if_none_match_it_does_not_meet(#[case] field: &str) {
    let h = harness().await;
    let uri = cached_wheel_uri(&h);

    let (status, headers, body) = get_bytes_with_headers(&h.state, &uri, &[("if-none-match", field)]).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::ETAG], wheel_etag());
    assert_eq!(body, WHEEL);
}
#[tokio::test]
async fn test_matching_if_none_match_answers_before_the_range_is_read() {
    let h = harness().await;
    let uri = cached_wheel_uri(&h);

    let conditional = [("if-none-match", &*wheel_etag()), ("range", "bytes=2-5")];
    let (status, headers, body) = get_bytes_with_headers(&h.state, &uri, &conditional).await;

    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(!headers.contains_key(header::CONTENT_RANGE));
    assert!(body.is_empty());
}
#[tokio::test]
async fn test_range_is_served_when_if_none_match_holds_other_bytes() {
    let h = harness().await;
    let uri = cached_wheel_uri(&h);

    let conditional = [("if-none-match", "\"0000\""), ("range", "bytes=2-5")];
    let (status, headers, body) = get_bytes_with_headers(&h.state, &uri, &conditional).await;

    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers[header::ETAG], wheel_etag());
    assert_eq!(headers[header::CONTENT_RANGE], format!("bytes 2-5/{}", WHEEL.len()));
    assert_eq!(body, &WHEEL[2..=5]);
}
#[tokio::test]
async fn test_matching_if_none_match_in_a_later_field_line_is_not_modified() {
    let h = harness().await;
    let uri = cached_wheel_uri(&h);

    let repeated = [("if-none-match", "\"0000\""), ("if-none-match", &*wheel_etag())];
    let (status, headers, body) = get_bytes_with_headers(&h.state, &uri, &repeated).await;

    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert_eq!(headers[header::ETAG], wheel_etag());
    assert!(body.is_empty());
}
#[tokio::test]
async fn test_repeated_range_lines_serve_the_whole_wheel_as_a_multi_range() {
    let h = harness().await;
    let uri = cached_wheel_uri(&h);

    let repeated = [("range", "bytes=0-1"), ("range", "bytes=4-5")];
    let (status, headers, body) = get_bytes_with_headers(&h.state, &uri, &repeated).await;

    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key(header::CONTENT_RANGE));
    assert_eq!(body, WHEEL);
}
#[tokio::test]
async fn test_repeated_if_modified_since_lines_serve_the_whole_wheel() {
    let h = harness().await;
    let uri = cached_wheel_uri(&h);
    let dated = wheel_last_modified(&h, &uri).await;

    let repeated = [("if-modified-since", &*dated), ("if-modified-since", &*dated)];
    let (status, headers, body) = get_bytes_with_headers(&h.state, &uri, &repeated).await;

    assert_eq!(status, StatusCode::OK, "a repeated singleton field states no condition");
    assert!(headers.contains_key(header::LAST_MODIFIED));
    assert_eq!(body, WHEEL);
}
#[tokio::test]
async fn test_matching_if_none_match_never_fetches_an_uncached_artifact() {
    let h = harness().await;
    let digest = Digest::of(WHEEL);
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_detail(&h.server, digest.as_str(), &file_url, None).await;
    Mock::given(method("GET"))
        .and(path("/files/flask.whl"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(WHEEL.to_vec()))
        .expect(0)
        .mount(&h.server)
        .await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    let uri = format!("/pypi/files/{}/flask-1.0-py3-none-any.whl", digest.as_str());
    let (status, headers, body) = get_bytes_with_headers(&h.state, &uri, &[("if-none-match", &wheel_etag())]).await;

    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert_eq!(headers[header::ETAG], wheel_etag());
    assert!(body.is_empty());
    assert!(h.state.serving.blobs.head(&digest).await.unwrap().is_none());
}
async fn wheel_last_modified(h: &Harness, uri: &str) -> String {
    let (_, headers, _) = get_bytes(&h.state, uri, None).await;
    headers[header::LAST_MODIFIED].to_str().unwrap().to_owned()
}

#[rstest]
#[case::get("GET")]
#[case::head("HEAD")]
#[tokio::test]
async fn test_cached_file_is_dated_by_the_write_that_cached_it(#[case] verb: &str) {
    let h = harness().await;
    let uri = cached_wheel_uri(&h);

    let (status, headers, _) = send_bytes(&h.state, verb, &uri, &[]).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers[header::LAST_MODIFIED],
        httpdate::fmt_http_date(
            h.state
                .serving
                .blobs
                .head(&Digest::of(WHEEL))
                .await
                .unwrap()
                .unwrap()
                .modified
                .unwrap()
        )
    );
}
#[tokio::test]
async fn test_an_artifact_arriving_from_upstream_is_dated_by_nothing() {
    let h = harness().await;
    let digest = Digest::of(WHEEL);
    let file_url = format!("{}/files/flask.whl", h.server.uri());
    mount_detail(&h.server, digest.as_str(), &file_url, None).await;
    Mock::given(method("GET"))
        .and(path("/files/flask.whl"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(WHEEL.to_vec()))
        .mount(&h.server)
        .await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;

    let uri = format!("/pypi/files/{}/flask-1.0-py3-none-any.whl", digest.as_str());
    let (status, headers, body) = get_bytes(&h.state, &uri, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, WHEEL, "the tee still serves the bytes it is caching");
    assert!(!headers.contains_key(header::LAST_MODIFIED), "no write to date it by");
}
#[tokio::test]
async fn test_cached_file_the_client_already_dated_is_not_modified() {
    let h = harness().await;
    let uri = cached_wheel_uri(&h);
    let dated = wheel_last_modified(&h, &uri).await;

    let (status, headers, body) = get_bytes_with_headers(&h.state, &uri, &[("if-modified-since", &dated)]).await;

    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert_eq!(headers[header::LAST_MODIFIED], dated);
    assert_eq!(headers[header::ETAG], wheel_etag());
    assert!(body.is_empty());
}
/// RFC 9112 s6.2 admits a `Content-Length` on a `304` only at the length the `200` would have sent,
/// and RFC 9111 s4.3.4 has a cache write the field over the entry it already holds. The `200` for a
/// wheel states the wheel's size, so the `304` states no length rather than its empty body's zero.
#[rstest]
#[case::tagged("if-none-match", &wheel_etag())]
#[case::dated("if-modified-since", "Fri, 31 Dec 2100 23:59:59 GMT")]
#[tokio::test]
async fn test_a_not_modified_wheel_states_no_length(#[case] field: &str, #[case] value: &str) {
    let h = harness().await;
    let uri = cached_wheel_uri(&h);
    let (_, served, _) = get_bytes(&h.state, &uri, None).await;

    let (status, headers, _) = get_bytes_with_headers(&h.state, &uri, &[(field, value)]).await;

    assert_eq!(
        (
            status,
            headers.contains_key(header::CONTENT_LENGTH),
            served[header::CONTENT_LENGTH].to_str().unwrap(),
        ),
        (StatusCode::NOT_MODIFIED, false, WHEEL.len().to_string().as_str())
    );
}

#[tokio::test]
async fn test_cached_file_is_not_modified_since_a_date_that_has_not_arrived() {
    let h = harness().await;
    let uri = cached_wheel_uri(&h);
    let (status, _, body) = get_bytes_with_headers(
        &h.state,
        &uri,
        &[("if-modified-since", "Fri, 31 Dec 2100 23:59:59 GMT")],
    )
    .await;

    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(body.is_empty());
}
#[rstest]
#[case::stale("Tue, 15 Nov 1994 08:12:31 GMT")]
#[case::malformed("last tuesday")]
#[tokio::test]
async fn test_cached_file_is_served_for_an_if_modified_since_it_does_not_meet(#[case] field: &str) {
    let h = harness().await;
    let uri = cached_wheel_uri(&h);

    let (status, headers, body) = get_bytes_with_headers(&h.state, &uri, &[("if-modified-since", field)]).await;

    assert_eq!(status, StatusCode::OK);
    assert!(headers.contains_key(header::LAST_MODIFIED));
    assert_eq!(body, WHEEL);
}
#[tokio::test]
async fn test_an_if_none_match_that_holds_other_bytes_settles_the_date_too() {
    let h = harness().await;
    let uri = cached_wheel_uri(&h);
    let dated = wheel_last_modified(&h, &uri).await;

    let both = [("if-none-match", "\"0000\""), ("if-modified-since", &*dated)];
    let (status, _, body) = get_bytes_with_headers(&h.state, &uri, &both).await;

    assert_eq!(status, StatusCode::OK, "the entity tag was asked first and refused");
    assert_eq!(body, WHEEL);
}
#[tokio::test]
async fn test_a_matching_if_none_match_settles_a_date_it_disagrees_with() {
    let h = harness().await;
    let uri = cached_wheel_uri(&h);

    let both = [
        ("if-none-match", &*wheel_etag()),
        ("if-modified-since", "Tue, 15 Nov 1994 08:12:31 GMT"),
    ];
    let (status, headers, body) = get_bytes_with_headers(&h.state, &uri, &both).await;

    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert_eq!(headers[header::ETAG], wheel_etag());
    assert!(body.is_empty());
}
#[tokio::test]
async fn test_a_current_if_modified_since_answers_before_the_range_is_read() {
    let h = harness().await;
    let uri = cached_wheel_uri(&h);
    let dated = wheel_last_modified(&h, &uri).await;

    let conditional = [("if-modified-since", &*dated), ("range", "bytes=2-5")];
    let (status, headers, body) = get_bytes_with_headers(&h.state, &uri, &conditional).await;

    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert!(!headers.contains_key(header::CONTENT_RANGE));
    assert!(body.is_empty());
}
#[tokio::test]
async fn test_cached_file_serves_a_range_an_if_range_still_names() {
    let h = harness().await;
    let uri = cached_wheel_uri(&h);

    let conditional = [("if-range", &*wheel_etag()), ("range", "bytes=2-5")];
    let (status, headers, body) = get_bytes_with_headers(&h.state, &uri, &conditional).await;

    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers[header::CONTENT_RANGE], format!("bytes 2-5/{}", WHEEL.len()));
    assert_eq!(body, &WHEEL[2..=5]);
}
#[rstest]
#[case::stale_tag("\"0000\"")]
#[case::weak_tag(&format!("W/{}", wheel_etag()))]
#[case::date("Wed, 21 Oct 2015 07:28:00 GMT")]
#[case::malformed("0000")]
#[tokio::test]
async fn test_cached_file_serves_the_whole_wheel_for_a_range_a_stale_if_range_asks_for(#[case] field: &str) {
    let h = harness().await;
    let uri = cached_wheel_uri(&h);

    let conditional = [("if-range", field), ("range", "bytes=2-5")];
    let (status, headers, body) = get_bytes_with_headers(&h.state, &uri, &conditional).await;

    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key(header::CONTENT_RANGE));
    assert_eq!(body, WHEEL);
}
// A stale copy earns the whole wheel rather than a `416`: the request is well formed, only the bytes
// behind it went stale.
#[tokio::test]
async fn test_stale_if_range_serves_the_whole_wheel_rather_than_refusing_the_range() {
    let h = harness().await;
    let uri = cached_wheel_uri(&h);

    let conditional = [("if-range", "\"0000\""), ("range", "bytes=99-100")];
    let (status, _, body) = get_bytes_with_headers(&h.state, &uri, &conditional).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, WHEEL);
}
#[tokio::test]
async fn test_if_range_without_a_range_is_ignored() {
    let h = harness().await;
    let uri = cached_wheel_uri(&h);

    let (status, headers, body) = get_bytes_with_headers(&h.state, &uri, &[("if-range", "\"0000\"")]).await;

    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key(header::CONTENT_RANGE));
    assert_eq!(body, WHEEL);
}

async fn uncached_wheel_uri(h: &Harness, published_size: Option<usize>) -> String {
    let digest = Digest::of(WHEEL);
    let size = published_size.map_or_else(String::new, |size| format!(",\"size\":{size}"));
    // PEP 700 makes `size` mandatory from 1.1, so a page that publishes none declares 1.0.
    let api_version = if published_size.is_some() { "1.1" } else { "1.0" };
    let metadata_digest = Digest::of(b"flask metadata");
    let detail = format!(
        "{{\"meta\":{{\"api-version\":\"{api_version}\"}},\"name\":\"flask\",\"versions\":[\"1.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\"url\":\"{}/files/flask.whl\",\
         \"hashes\":{{\"sha256\":\"{}\"}},\"core-metadata\":{{\"sha256\":\"{}\"}}{size}}}]}}",
        h.server.uri(),
        digest.as_str(),
        metadata_digest.as_str()
    );
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(detail.into_bytes(), "application/vnd.pypi.simple.v1+json"),
        )
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/files/flask.whl"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(WHEEL.to_vec()))
        .expect(0)
        .mount(&h.server)
        .await;
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    format!("/pypi/files/{}/flask-1.0-py3-none-any.whl", digest.as_str())
}

#[tokio::test]
async fn test_head_of_an_uncached_file_on_an_offline_mirror_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let digest = Digest::of(WHEEL);
    crate::tests::register_publication(&meta, "pypi", "flask-1.0-py3-none-any.whl", digest.as_str(), None);
    crate::store::PypiStore::put_file_url(&meta, digest.as_str(), "https://files.example/flask.whl", "pypi").unwrap();
    let indexes = vec![Index {
        name: "pypi".to_owned(),
        route: "pypi".to_owned(),
        ecosystem: crate::ECOSYSTEM,
        kind: IndexKind::Cached {
            client: UpstreamClient::new("https://files.example/simple/").unwrap(),
            offline: true,
        },
        policy: Policy::default(),
        acl: IndexAcl::default(),
    }];
    let state = crate::tests::wired(AppState::new(
        meta,
        BlobStorage::filesystem(dir.path().join("blobs")),
        60,
        indexes,
    ));
    let uri = format!("/pypi/files/{}/flask-1.0-py3-none-any.whl", digest.as_str());

    let (status, _, body) = send_bytes(&state, "HEAD", &uri, &[]).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(body.is_empty());
}

#[tokio::test]
async fn test_head_of_an_uncached_file_never_fetches_the_artifact() {
    let h = harness().await;
    let uri = uncached_wheel_uri(&h, None).await;

    let (status, _, body) = send_bytes(&h.state, "HEAD", &uri, &[]).await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.is_empty());
    assert!(h.state.serving.blobs.head(&Digest::of(WHEEL)).await.unwrap().is_none());
}
#[tokio::test]
async fn test_head_of_an_uncached_file_carries_the_headers_of_its_download() {
    let h = harness().await;
    let uri = uncached_wheel_uri(&h, None).await;

    let (_, headers, _) = send_bytes(&h.state, "HEAD", &uri, &[]).await;

    assert_eq!(headers[header::CONTENT_TYPE], "application/octet-stream");
    assert_eq!(headers[header::ETAG], wheel_etag());
    assert_eq!(headers[header::ACCEPT_RANGES], "bytes");
    assert_eq!(
        headers[header::CACHE_CONTROL],
        format!(
            "public, max-age={}, must-revalidate, no-transform",
            peryx_driver::revocations::DECISION_CACHE_TTL_SECS,
        )
    );
}
#[tokio::test]
async fn test_head_of_an_uncached_file_states_the_length_its_index_page_published() {
    let h = harness().await;
    let uri = uncached_wheel_uri(&h, Some(WHEEL.len())).await;

    let (_, headers, _) = send_bytes(&h.state, "HEAD", &uri, &[]).await;

    assert_eq!(headers[header::CONTENT_LENGTH], WHEEL.len().to_string());
}
#[tokio::test]
async fn test_head_of_an_uncached_file_omits_a_length_no_index_page_published() {
    let h = harness().await;
    let uri = uncached_wheel_uri(&h, None).await;

    let (_, headers, _) = send_bytes(&h.state, "HEAD", &uri, &[]).await;

    assert!(!headers.contains_key(header::CONTENT_LENGTH));
}
// An uncached file is teed from upstream, and its GET serves the whole representation rather than slice
// a body it cannot seek. The HEAD promises what that GET delivers.
#[tokio::test]
async fn test_head_of_an_uncached_file_answers_a_range_with_the_whole_representation() {
    let h = harness().await;
    let uri = uncached_wheel_uri(&h, Some(WHEEL.len())).await;

    let (status, headers, _) = send_bytes(&h.state, "HEAD", &uri, &[("range", "bytes=2-5")]).await;

    assert_eq!(status, StatusCode::OK);
    assert!(!headers.contains_key(header::CONTENT_RANGE));
    assert_eq!(headers[header::CONTENT_LENGTH], WHEEL.len().to_string());
}
#[tokio::test]
async fn test_head_of_a_file_no_index_registered_is_not_found() {
    let h = harness().await;
    let uri = format!(
        "/pypi/files/{}/flask-1.0-py3-none-any.whl",
        Digest::of(b"unknown").as_str()
    );

    let (status, _, body) = send_bytes(&h.state, "HEAD", &uri, &[]).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body.is_empty());
}
#[rstest]
#[case::whole(&[])]
#[case::ranged(&[("range", "bytes=2-5")])]
#[case::unsatisfiable(&[("range", "bytes=99-100")])]
#[case::not_modified(&[("if-modified-since", "Fri, 31 Dec 2100 23:59:59 GMT")])]
#[case::modified(&[("if-modified-since", "Tue, 15 Nov 1994 08:12:31 GMT")])]
#[case::not_modified_over_a_range(&[
    ("if-modified-since", "Fri, 31 Dec 2100 23:59:59 GMT"),
    ("range", "bytes=2-5"),
])]
#[tokio::test]
async fn test_head_of_a_cached_file_answers_what_its_get_would(#[case] extra_headers: &[(&str, &str)]) {
    let h = harness().await;
    let uri = cached_wheel_uri(&h);

    let (status, headers, body) = send_bytes(&h.state, "HEAD", &uri, extra_headers).await;
    let (get_status, get_headers, _) = get_bytes_with_headers(&h.state, &uri, extra_headers).await;

    assert_eq!(status, get_status);
    assert_eq!(headers, get_headers);
    assert!(body.is_empty());
}
#[tokio::test]
async fn test_file_path_without_filename_is_not_found() {
    let h = harness().await;
    let (status, ..) = get(&h.state, "/pypi/files/onlyonesegment", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
#[tokio::test]
async fn test_removal_storage_error_is_server_error() {
    let h = harness().await;
    h.state
        .serving
        .meta
        .put_upload("hosted", "peryxpkg", "peryxpkg-1.0.whl", b"{ not json")
        .unwrap();
    // A versioned delete must decode each record to filter, so the corrupt record errors.
    let status = request(&h.state, "DELETE", "/hosted/peryxpkg/1.0/", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}
