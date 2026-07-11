//! Yank, unyank, delete and restore, and the admin routes that drive them.

use super::support::*;

#[tokio::test]
async fn test_yank_and_unyank_and_delete() {
    let h = harness().await;
    upload_peryxpkg(&h.state, "/root/pypi/", &fixture_wheel()).await;

    // Yank the version, then the file is served with the recorded reason.
    assert_eq!(
        request(
            &h.state,
            "PUT",
            "/root/pypi/peryxpkg/1.0/yank?ignored=1&reason=bad+build",
            Some(&upload_auth())
        )
        .await,
        StatusCode::OK
    );
    let (_, _, yanked) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert!(yanked.contains("\"yanked\":\"bad build\""));

    // Un-yank via DELETE .../yank.
    assert_eq!(
        request(&h.state, "DELETE", "/root/pypi/peryxpkg/1.0/yank", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (_, _, unyanked) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert!(!unyanked.contains("\"yanked\":true"));

    // Delete the whole project.
    assert_eq!(
        request(&h.state, "DELETE", "/root/pypi/peryxpkg/", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (status, ..) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
#[tokio::test]
async fn test_delete_specific_version() {
    let h = harness().await;
    upload_peryxpkg(&h.state, "/hosted/", &fixture_wheel()).await;
    assert_eq!(
        request(&h.state, "DELETE", "/hosted/peryxpkg/1.0/", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (status, ..) = get(&h.state, "/hosted/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
#[tokio::test]
async fn test_admin_routes_decode_safe_project_and_version_segments() {
    let h = harness().await;
    upload_version(&h.state, "/hosted/", "1.0+local").await;
    assert_eq!(
        request(
            &h.state,
            "DELETE",
            "/hosted/peryxpkg/1.0%2Blocal/",
            Some(&upload_auth())
        )
        .await,
        StatusCode::OK
    );
    let (status, ..) = get(&h.state, "/hosted/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
#[tokio::test]
async fn test_admin_routes_reject_decoded_separators() {
    let h = harness().await;
    assert_eq!(
        request(&h.state, "DELETE", "/hosted/velo%2Fdexpkg/", Some(&upload_auth())).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(&h.state, "DELETE", "/hosted/peryxpkg/1.0%2Fbad/", Some(&upload_auth())).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(&h.state, "DELETE", "/hosted/velo%xxdexpkg/", Some(&upload_auth())).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(&h.state, "DELETE", "/hosted/peryxpkg/1.0%xxbad/", Some(&upload_auth())).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(&h.state, "PUT", "/hosted/velo%2Fdexpkg/yank", Some(&upload_auth())).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(&h.state, "PUT", "/hosted/velo%2Fdexpkg/restore", Some(&upload_auth())).await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(&h.state, "DELETE", "/hosted/velo%2Fdexpkg/yank", Some(&upload_auth())).await,
        StatusCode::BAD_REQUEST
    );
}
#[tokio::test]
async fn test_delete_nonexistent_is_not_found() {
    let h = harness().await;
    assert_eq!(
        request(&h.state, "DELETE", "/hosted/ghost/", Some(&upload_auth())).await,
        StatusCode::NOT_FOUND
    );
}
#[tokio::test]
async fn test_delete_requires_auth() {
    let h = harness().await;
    upload_peryxpkg(&h.state, "/hosted/", &fixture_wheel()).await;
    assert_eq!(
        request(&h.state, "DELETE", "/hosted/peryxpkg/", None).await,
        StatusCode::UNAUTHORIZED
    );
}
#[tokio::test]
async fn test_delete_on_non_volatile_is_forbidden() {
    let h = harness_with(true, false).await;
    upload_peryxpkg(&h.state, "/hosted/", &fixture_wheel()).await;
    let (status, body) = request_response(&h.state, "DELETE", "/hosted/peryxpkg/", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body, "file removal: index is not volatile; delete is disabled");
}
#[tokio::test]
async fn test_delete_on_mirror_route_is_method_not_allowed() {
    let h = harness().await;
    assert_eq!(
        request(&h.state, "DELETE", "/pypi/flask/", Some(&upload_auth())).await,
        StatusCode::METHOD_NOT_ALLOWED
    );
}
#[tokio::test]
async fn test_yank_on_mirror_route_is_method_not_allowed() {
    let h = harness().await;
    let status = request(&h.state, "PUT", "/pypi/flask/1.0/yank", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}
#[tokio::test]
async fn test_delete_one_of_two_versions() {
    let h = harness().await;
    upload_version(&h.state, "/hosted/", "1.0").await;
    upload_version(&h.state, "/hosted/", "2.0").await;
    assert_eq!(
        request(&h.state, "DELETE", "/hosted/peryxpkg/1.0/", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (_, _, detail) = get(&h.state, "/hosted/simple/peryxpkg/", Some("application/json")).await;
    assert!(detail.contains("2.0"));
    assert!(!detail.contains("peryxpkg-1.0"));
}
#[tokio::test]
async fn test_yank_one_of_two_versions() {
    let h = harness().await;
    upload_version(&h.state, "/hosted/", "1.0").await;
    upload_version(&h.state, "/hosted/", "2.0").await;
    assert_eq!(
        request(&h.state, "PUT", "/hosted/peryxpkg/1.0/yank", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (_, _, detail) = get(&h.state, "/hosted/simple/peryxpkg/", Some("application/json")).await;
    // Only the 1.0 file carries the yank marker.
    assert_eq!(detail.matches("\"yanked\":true").count(), 1);
}
#[tokio::test]
async fn test_yank_upstream_file_via_overlay() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    mount_detail(&h.server, digest.as_str(), "http://x/flask-1.0-py3-none-any.whl", None).await;

    let status = request(
        &h.state,
        "PUT",
        "/root/pypi/flask/1.0/yank?reason=bad+build",
        Some(&upload_auth()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The virtual index page carries the marker; the cache's own route stays untouched.
    let (_, _, merged) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;
    assert!(merged.contains("\"yanked\":\"bad build\""));
    let (_, _, cached) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    assert!(!cached.contains("\"yanked\":\"bad build\""));

    // Un-yank clears the override.
    let status = request(&h.state, "DELETE", "/root/pypi/flask/1.0/yank", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, cleared) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;
    assert!(!cleared.contains("\"yanked\":true"));

    let status = request(&h.state, "PUT", "/root/pypi/flask/1.0/yank", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, yanked) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;
    assert!(yanked.contains("\"yanked\":true"));
}
#[tokio::test]
async fn test_delete_and_restore_upstream_file_via_overlay() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    mount_detail(&h.server, digest.as_str(), "http://x/flask-1.0-py3-none-any.whl", None).await;

    let status = request(&h.state, "DELETE", "/root/pypi/flask/", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::OK);

    // Hidden from the virtual index page, but still present on the cache's own route.
    let (_, _, merged) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;
    assert!(!merged.contains("flask-1.0-py3-none-any.whl"));
    let (_, _, cached) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    assert!(cached.contains("flask-1.0-py3-none-any.whl"));

    // Restore brings the file back.
    let status = request(&h.state, "PUT", "/root/pypi/flask/restore", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, restored) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;
    assert!(restored.contains("flask-1.0-py3-none-any.whl"));
}
#[tokio::test]
async fn test_delete_one_upstream_version_leaves_other() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    let json = format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"flask\",\"versions\":[\"1.0\",\"2.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\"url\":\"http://x/a.whl\",\
         \"hashes\":{{\"sha256\":\"{digest}\"}}}},\
         {{\"filename\":\"flask-2.0-py3-none-any.whl\",\"url\":\"http://x/b.whl\",\
         \"hashes\":{{\"sha256\":\"{digest}\"}}}}]}}",
        digest = digest.as_str()
    );
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(json.into_bytes(), "application/vnd.pypi.simple.v1+json"))
        .mount(&h.server)
        .await;

    let status = request(&h.state, "DELETE", "/root/pypi/flask/1.0/", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, merged) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;
    assert!(!merged.contains("flask-1.0-py3-none-any.whl"));
    assert!(merged.contains("flask-2.0-py3-none-any.whl"));
}
#[tokio::test]
async fn test_restore_with_nothing_hidden_is_not_found() {
    let h = harness().await;
    let digest = Digest::of(b"wheel");
    mount_detail(&h.server, digest.as_str(), "http://x/flask-1.0-py3-none-any.whl", None).await;
    let status = request(&h.state, "PUT", "/root/pypi/flask/restore", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
#[tokio::test]
async fn test_delete_upstream_on_non_volatile_still_hides() {
    let h = harness_with(true, false).await;
    let digest = Digest::of(b"wheel");
    mount_detail(&h.server, digest.as_str(), "http://x/flask-1.0-py3-none-any.whl", None).await;
    // Hiding an upstream file is reversible, so it works even when uploads are immutable.
    let status = request(&h.state, "DELETE", "/root/pypi/flask/", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::OK);
}
#[tokio::test]
async fn test_yank_overlay_with_uploaded_file_skips_override() {
    let h = harness().await;
    Mock::given(method("GET"))
        .and(path("/simple/peryxpkg/"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&h.server)
        .await;
    upload_wheel(&h.state, "peryxpkg-1.0-py3-none-any.whl", &fixture_wheel()).await;
    // Yank through the virtual index: the uploaded file is rewritten, no override is created.
    assert_eq!(
        request(&h.state, "PUT", "/root/pypi/peryxpkg/1.0/yank", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (_, _, detail) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert!(detail.contains("\"yanked\":true"));
    // A second identical yank changes nothing: uploaded state already matches, override skip too.
    let status = request(&h.state, "PUT", "/root/pypi/peryxpkg/1.0/yank", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Un-yank with no upstream override to clear only rewrites the record.
    assert_eq!(
        request(&h.state, "DELETE", "/root/pypi/peryxpkg/1.0/yank", Some(&upload_auth())).await,
        StatusCode::OK
    );
}
#[tokio::test]
async fn test_versioned_delete_matches_upload_record_when_filename_lacks_version() {
    let h = harness().await;
    // The filename carries no parsable version, so the served-page filter misses it and the
    // record-based fallback deletes by the version stored at upload time.
    put_local_file(&h.state, "peryxpkg.whl", b"payload", "9.9");
    assert_eq!(
        request(&h.state, "DELETE", "/hosted/peryxpkg/9.9/", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (status, ..) = get(&h.state, "/hosted/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
#[tokio::test]
async fn test_versioned_delete_fallback_skips_other_versions() {
    let h = harness().await;
    // Neither filename carries a parsable version, so both deletes go through the record fallback.
    for (version, filename) in [("1.5", "peryxpkg-one.whl"), ("2.5", "peryxpkg-two.whl")] {
        put_local_file(&h.state, filename, format!("payload {version}").as_bytes(), version);
    }
    assert_eq!(
        request(&h.state, "DELETE", "/hosted/peryxpkg/1.5/", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (_, _, detail) = get(&h.state, "/hosted/simple/peryxpkg/", Some("application/json")).await;
    assert!(detail.contains("peryxpkg-two.whl"));
    assert!(!detail.contains("peryxpkg-one.whl"));
}
#[tokio::test]
async fn test_restore_skips_yanked_overrides_and_other_versions() {
    let h = harness().await;
    h.state
        .meta
        .put_override("hosted", "flask", "flask-1.0-py3-none-any.whl", "yanked")
        .unwrap();
    h.state
        .meta
        .put_override("hosted", "flask", "flask-2.0-py3-none-any.whl", "hidden")
        .unwrap();
    // Restoring 1.0 touches nothing: its override is a yank, and the hidden file is another version.
    let status = request(&h.state, "PUT", "/root/pypi/flask/1.0/restore", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // The hidden 2.0 override is still there to restore.
    let status = request(&h.state, "PUT", "/root/pypi/flask/2.0/restore", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::OK);
}
#[tokio::test]
async fn test_yank_with_corrupt_record_is_server_error() {
    let h = harness().await;
    h.state
        .meta
        .put_upload("hosted", "peryxpkg", "peryxpkg-1.0.whl", b"{ not json")
        .unwrap();
    let status = request(&h.state, "PUT", "/hosted/peryxpkg/1.0/yank", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}
