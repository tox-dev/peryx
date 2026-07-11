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
