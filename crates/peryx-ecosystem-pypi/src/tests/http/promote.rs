//! Promoting a release from one index to another.

use super::support::*;

#[tokio::test]
async fn test_promote_requires_source_query() {
    let h = harness().await;
    let (status, body) = request_response(&h.state, "PUT", "/root/pypi/flask/1.0/promote", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, "promotion requires from={source route}");

    let (status, body) = request_response(
        &h.state,
        "PUT",
        "/root/pypi/flask/1.0/promote?source=local",
        Some(&upload_auth()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, "promotion requires from={source route}");
}
#[tokio::test]
async fn test_promote_requires_version() {
    let h = promotion_harness().await;
    let (status, body) = request_response(
        &h.state,
        "PUT",
        "/prod/peryxpkg/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body, "promotion requires a version");
}
#[tokio::test]
async fn test_promote_rejects_invalid_project_path() {
    let h = promotion_harness().await;
    let (status, body) = request_response(
        &h.state,
        "PUT",
        "/prod/peryxpkg%2Fbad/1.0/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body,
        "invalid project \"peryxpkg/bad\": path parameters must be non-empty segments without separators, \
         traversal, or control characters"
    );
}
#[tokio::test]
async fn test_promote_copies_release_records_without_copying_blobs() {
    let h = promotion_harness().await;
    let wheel = fixture_wheel();
    let digest = upload_wheel_to(&h.state, "/staging/", "peryxpkg-1.0-py3-none-any.whl", "1.0", &wheel).await;
    upload_wheel_to(
        &h.state,
        "/staging/",
        "peryxpkg-2.0-py3-none-any.whl",
        "2.0",
        &fixture_wheel_with_body("2.0", b"VALUE = 2\n"),
    )
    .await;
    assert_eq!(
        request(
            &h.state,
            "PUT",
            "/staging/peryxpkg/1.0/yank?reason=bad+build",
            Some(&upload_auth()),
        )
        .await,
        StatusCode::OK
    );
    let blobs_before = blob_count(&h.state);

    let (status, body) = request_response(
        &h.state,
        "PUT",
        "/prod/peryxpkg/1.0/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "promoted 1 file(s)");
    assert_eq!(blob_count(&h.state), blobs_before);
    let (_, _, body) = get(&h.state, "/prod/simple/peryxpkg/", Some("application/json")).await;
    let detail: serde_json::Value = serde_json::from_str(&body).unwrap();
    let file = &detail["files"][0];
    assert_eq!(
        file,
        &serde_json::json!({
            "filename": "peryxpkg-1.0-py3-none-any.whl",
            "url": format!("/prod/files/{}/peryxpkg-1.0-py3-none-any.whl", digest.as_str()),
            "hashes": {"sha256": digest.as_str()},
            "requires-python": ">=3.8",
            "size": wheel.len() as u64,
            "upload-time": "1970-01-01T00:16:40Z",
            "yanked": "bad build",
            "core-metadata": file["core-metadata"].clone(),
            "dist-info-metadata": file["core-metadata"].clone()
        })
    );
    assert!(
        file["core-metadata"]["sha256"]
            .as_str()
            .is_some_and(|sha256| sha256.len() == 64)
    );
    let metadata_uri = format!("/prod/files/{}/peryxpkg-1.0-py3-none-any.whl.metadata", digest.as_str());
    let (metadata_status, _, metadata) = get(&h.state, &metadata_uri, None).await;
    assert_eq!(metadata_status, StatusCode::OK);
    assert!(metadata.contains("Name: peryxpkg"));
    assert_eq!(detail["files"].as_array().unwrap().len(), 1);
}
#[tokio::test]
async fn test_promote_matches_source_by_pep440_equality() {
    let h = promotion_harness().await;
    // Staged with form version 1.0; a promote addressed to the PEP 440-equal 1.0.0 must still copy it.
    upload_wheel_to(
        &h.state,
        "/staging/",
        "peryxpkg-1.0-py3-none-any.whl",
        "1.0",
        &fixture_wheel(),
    )
    .await;
    let (status, body) = request_response(
        &h.state,
        "PUT",
        "/prod/peryxpkg/1.0.0/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "promoted 1 file(s)");
}
#[tokio::test]
async fn test_promote_skips_target_file_with_same_digest() {
    let h = promotion_harness().await;
    let wheel = fixture_wheel();
    upload_wheel_to(&h.state, "/staging/", "peryxpkg-1.0-py3-none-any.whl", "1.0", &wheel).await;
    upload_wheel_to(&h.state, "/prod/", "peryxpkg-1.0-py3-none-any.whl", "1.0", &wheel).await;
    let logs = LogCapture::default();
    let _guard = logs.install();

    let (status, body) = request_response(
        &h.state,
        "PUT",
        "/prod/peryxpkg/1.0/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;

    let event = logs
        .security_events()
        .into_iter()
        .find(|event| field(event, "action") == Some("promote"))
        .unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "promoted 0 file(s)");
    assert_eq!(field(&event, "action"), Some("promote"));
    assert_eq!(field(&event, "result"), Some("noop"));
    assert_eq!(field(&event, "index"), Some("prod"));
    assert_eq!(field(&event, "source_index"), Some("staging"));
    assert_eq!(field(&event, "reason"), Some("same files already exist on target"));
}
#[tokio::test]
async fn test_promote_reports_missing_sha256_in_source_record() {
    let h = promotion_harness().await;
    let filename = "peryxpkg-1.0-py3-none-any.whl";
    let uploaded = upload_record(
        filename,
        "1.0",
        "https://example.test/pkg.whl".to_owned(),
        BTreeMap::new(),
        Some(4),
    );
    h.state
        .meta
        .put_upload("staging", "peryxpkg", filename, &to_json(&uploaded).into_bytes())
        .unwrap();
    h.state.meta.put_project("staging", "peryxpkg", "peryxpkg").unwrap();
    let logs = LogCapture::default();
    let _guard = logs.install();

    let (status, body) = request_response(
        &h.state,
        "PUT",
        "/prod/peryxpkg/1.0/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;

    let event = logs
        .security_events()
        .into_iter()
        .find(|event| field(event, "action") == Some("promote"))
        .unwrap();
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        body,
        "promotion: uploaded file \"peryxpkg-1.0-py3-none-any.whl\" has no sha256 hash"
    );
    assert_eq!(field(&event, "result"), Some("failure"));
    assert_eq!(
        field(&event, "reason"),
        Some("uploaded file \"peryxpkg-1.0-py3-none-any.whl\" has no sha256 hash")
    );
}
#[tokio::test]
async fn test_promote_uses_normalized_name_when_source_display_is_missing() {
    let h = promotion_harness().await;
    let filename = "peryxpkg-1.0-py3-none-any.whl";
    let digest = Digest::of(b"wheel");
    let uploaded = upload_record(
        filename,
        "1.0",
        local_file_url("staging", digest.as_str(), filename),
        BTreeMap::from([("sha256".to_owned(), digest.as_str().to_owned())]),
        Some(5),
    );
    h.state
        .meta
        .put_upload("staging", "peryxpkg", filename, &to_json(&uploaded).into_bytes())
        .unwrap();

    let (status, body) = request_response(
        &h.state,
        "PUT",
        "/prod/peryxpkg/1.0/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;

    let (_, _, detail) = get(&h.state, "/prod/simple/peryxpkg/", Some("application/json")).await;
    let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "promoted 1 file(s)");
    assert_eq!(detail["name"], "peryxpkg");
}
#[tokio::test]
async fn test_promote_reports_no_matching_source_release() {
    let h = promotion_harness().await;

    let (status, body) = request_response(
        &h.state,
        "PUT",
        "/prod/peryxpkg/1.0/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        body,
        "promotion: no uploaded files on source \"staging\" match project \"peryxpkg\" version \"1.0\""
    );
}
#[tokio::test]
async fn test_promote_conflicts_on_target_filename_with_different_bytes() {
    let h = promotion_harness().await;
    upload_wheel_to(
        &h.state,
        "/staging/",
        "peryxpkg-1.0-py3-none-any.whl",
        "1.0",
        &fixture_wheel(),
    )
    .await;
    upload_wheel_to(
        &h.state,
        "/prod/",
        "peryxpkg-1.0-py3-none-any.whl",
        "1.0",
        &fixture_wheel_with_body("1.0", b"VALUE = 2\n"),
    )
    .await;

    let (status, body) = request_response(
        &h.state,
        "PUT",
        "/prod/peryxpkg/1.0/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body,
        "File already exists: \"peryxpkg-1.0-py3-none-any.whl\" has different content; use a different filename"
    );
}
#[tokio::test]
async fn test_promote_rejects_invalid_source_and_target_routes() {
    let h = promotion_harness().await;
    upload_wheel_to(
        &h.state,
        "/staging/",
        "peryxpkg-1.0-py3-none-any.whl",
        "1.0",
        &fixture_wheel(),
    )
    .await;

    assert_eq!(
        request(
            &h.state,
            "PUT",
            "/missing/peryxpkg/1.0/promote?from=staging",
            Some(&upload_auth()),
        )
        .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            &h.state,
            "PUT",
            "/prod/peryxpkg/1.0/promote?from=missing",
            Some(&upload_auth()),
        )
        .await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(
            &h.state,
            "PUT",
            "/prod/peryxpkg/1.0/promote?from=pypi",
            Some(&upload_auth()),
        )
        .await,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        request(
            &h.state,
            "PUT",
            "/pypi/peryxpkg/1.0/promote?from=staging",
            Some(&upload_auth()),
        )
        .await,
        StatusCode::METHOD_NOT_ALLOWED
    );
}
#[tokio::test]
async fn test_promote_rejects_archived_and_quarantined_targets() {
    for status in ["archived", "quarantined"] {
        let h = promotion_harness().await;
        upload_wheel_to(
            &h.state,
            "/staging/",
            "peryxpkg-1.0-py3-none-any.whl",
            "1.0",
            &fixture_wheel(),
        )
        .await;
        let digest = Digest::of(b"upstream");
        let file_url = format!("{}/files/peryxpkg.whl", h.server.uri());
        mount_status_detail(&h.server, "peryxpkg", status, "policy", digest.as_str(), &file_url).await;

        let (code, body) = request_response(
            &h.state,
            "PUT",
            "/release/peryxpkg/1.0/promote?from=staging",
            Some(&upload_auth()),
        )
        .await;

        assert_eq!(code, StatusCode::FORBIDDEN);
        assert_eq!(body, format!("project \"peryxpkg\" is {status}; uploads are disabled"));
        assert_eq!(
            get(&h.state, "/prod/simple/peryxpkg/", Some("application/json"))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
    }
}
