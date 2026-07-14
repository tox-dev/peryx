//! Serving an artifact by digest: fetch, verify, cache, and reject.

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

    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await; // registers the file url
    let uri = format!("/pypi/files/{}/flask-1.0-py3-none-any.whl", digest.as_str());
    let (status, _, body) = get(&h.state, &uri, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "wheelcontent");
    let (status2, _, body2) = get(&h.state, &uri, None).await; // second from blob cache
    assert_eq!(status2, StatusCode::OK);
    assert_eq!(body2, body);
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
    assert_eq!(detail["meta"]["project-status"], "quarantined");
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
    assert!(body.contains("filenames must be relative path segments"));
}
#[tokio::test]
async fn test_file_download_allows_literal_percent_filename() {
    let h = harness().await;
    let digest = put_local_file(&h.state, "peryxpkg%2F.whl", b"PKpercent", "1.0");
    let uri = format!("/hosted/files/{}/peryxpkg%252F.whl", digest.as_str());
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
    h.state
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
    // The transfer fails verification, so the body errors instead of completing…
    let response = router(h.state.clone())
        .oneshot(Request::builder().uri(&*uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.into_body().collect().await.is_err());
    // …the corrupt blob is never admitted into the store, and the rejection is counted. The poll
    // must yield to the runtime: the detached transfer task records the rejection, and a blocking
    // sleep would starve it on the single-threaded test runtime.
    for _ in 0..500 {
        let totals = h.state.metrics.index_totals();
        if totals.get("pypi").is_some_and(|t| t.base.rejected == 1) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    assert!(!h.state.blobs.exists(&digest));
    assert_eq!(h.state.metrics.index_totals()["pypi"].base.rejected, 1);
}
const WHEEL: &[u8] = b"wheelcontent";

fn cached_wheel_uri(h: &Harness) -> String {
    let digest = Digest::of(WHEEL);
    h.state.blobs.write_verified(WHEEL, &digest).unwrap();
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
#[case::open_ended("bytes=6-", 6, 11)]
#[case::suffix("bytes=-4", 8, 11)]
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
    assert_eq!(headers[header::CACHE_CONTROL], "public, max-age=31536000, immutable");
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
    get(&h.state, "/pypi/simple/flask/", Some("application/json")).await; // registers the file url

    let uri = format!("/pypi/files/{}/flask-1.0-py3-none-any.whl", digest.as_str());
    let (status, headers, body) = get_bytes_with_headers(&h.state, &uri, &[("if-none-match", &wheel_etag())]).await;

    assert_eq!(status, StatusCode::NOT_MODIFIED);
    assert_eq!(headers[header::ETAG], wheel_etag());
    assert!(body.is_empty());
    assert!(!h.state.blobs.exists(&digest));
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
        .meta
        .put_upload("hosted", "peryxpkg", "peryxpkg-1.0.whl", b"{ not json")
        .unwrap();
    // A versioned delete must decode each record to filter, so the corrupt record errors.
    let status = request(&h.state, "DELETE", "/hosted/peryxpkg/1.0/", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}
