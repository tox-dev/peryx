use super::support::*;
use crate::store::read_journal_entries;
use peryx_core::path::local_artifact_url;

fn store_ambiguous_sdist(state: &AppState, index: &str) -> (&'static str, Digest) {
    let filename = "peryxpkg-1.0-1.tar.gz";
    let payload = b"historical sdist";
    let digest = Digest::of(payload);
    state.serving.blobs.blocking().put_bytes_as(payload, &digest).unwrap();
    let mut uploaded = upload_record(
        filename,
        "1.0-1",
        local_artifact_url(index, digest.as_str(), filename),
        BTreeMap::from([("sha256".to_owned(), digest.as_str().to_owned())]),
        Some(payload.len() as u64),
    );
    uploaded.imports = Some(crate::upload::ImportDeclarations::Before25);
    uploaded.file.upload_time = Some("2025-01-01T00:00:00Z".to_owned());
    state
        .serving
        .meta
        .put_upload(index, "peryxpkg", filename, crate::to_json(&uploaded).as_bytes())
        .unwrap();
    state.serving.meta.put_project(index, "peryxpkg", "peryxpkg").unwrap();
    (filename, digest)
}

#[tokio::test]
async fn test_yank_and_unyank_and_delete() {
    let h = authority_harness().await;
    upload_peryxpkg(&h.state, "/root/pypi/", &fixture_wheel()).await;

    h.clock.store(2000, Ordering::Relaxed);
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
    let restarted = restarted_state(&h);
    let (_, _, yanked) = get(&restarted, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert!(yanked.contains("\"yanked\":\"bad build\""));

    h.clock.store(3000, Ordering::Relaxed);
    assert_eq!(
        request(&h.state, "DELETE", "/root/pypi/peryxpkg/1.0/yank", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (_, _, unyanked) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert!(!unyanked.contains("\"yanked\":true"));

    h.clock.store(4000, Ordering::Relaxed);
    assert_eq!(
        request(&h.state, "DELETE", "/root/pypi/peryxpkg/", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (status, ..) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        read_journal_entries(&h.state.serving.meta, 0, 10)
            .unwrap()
            .entries
            .into_iter()
            .map(|entry| (entry.action, entry.version, entry.filename, entry.submitted_at_unix))
            .collect::<Vec<_>>(),
        std::iter::once(("new-release".to_owned(), Some("1.0".to_owned()), None, 1000,))
            .chain(
                [
                    ("add-file", 1000),
                    ("withdraw", 2000),
                    ("unyank", 3000),
                    ("delete-file", 4000),
                ]
                .map(|(action, submitted)| {
                    (
                        action.to_owned(),
                        Some("1.0".to_owned()),
                        Some("peryxpkg-1.0-py3-none-any.whl".to_owned()),
                        submitted,
                    )
                }),
            )
            .collect::<Vec<_>>()
    );
}
#[tokio::test]
async fn test_delete_specific_version() {
    let h = authority_harness().await;
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
    let h = authority_harness().await;
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
    let h = authority_harness().await;
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
    let h = authority_harness().await;
    assert_eq!(
        request(&h.state, "DELETE", "/hosted/ghost/", Some(&upload_auth())).await,
        StatusCode::NOT_FOUND
    );
}
#[tokio::test]
async fn test_delete_requires_auth() {
    let h = authority_harness().await;
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
    let h = authority_harness().await;
    assert_eq!(
        request(&h.state, "DELETE", "/pypi/flask/", Some(&upload_auth())).await,
        StatusCode::METHOD_NOT_ALLOWED
    );
}
#[tokio::test]
async fn test_yank_on_mirror_route_is_method_not_allowed() {
    let h = authority_harness().await;
    let status = request(&h.state, "PUT", "/pypi/flask/1.0/yank", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}
#[tokio::test]
async fn test_delete_one_of_two_versions() {
    let h = authority_harness().await;
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
    let h = authority_harness().await;
    upload_version(&h.state, "/hosted/", "1.0").await;
    upload_version(&h.state, "/hosted/", "2.0").await;
    assert_eq!(
        request(&h.state, "PUT", "/hosted/peryxpkg/1.0/yank", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (_, _, detail) = get(&h.state, "/hosted/simple/peryxpkg/", Some("application/json")).await;

    assert_eq!(detail.matches("\"yanked\":true").count(), 1);
}
#[tokio::test]
async fn test_yank_matches_upload_by_pep440_equality() {
    let h = authority_harness().await;
    // Mutation lookup uses PEP 440 equality, not string equality.
    put_local_file(&h.state, "peryxpkg-1.0-py3-none-any.whl", b"payload", "1.0");
    assert_eq!(
        request(&h.state, "PUT", "/hosted/peryxpkg/1.0.0/yank", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (_, _, detail) = get(&h.state, "/hosted/simple/peryxpkg/", Some("application/json")).await;
    assert!(detail.contains("\"yanked\":true"));
}
#[tokio::test]
async fn test_yank_upstream_file_via_overlay() {
    let h = authority_harness().await;
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

    let (_, _, merged) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;
    assert!(merged.contains("\"yanked\":\"bad build\""));
    let (_, _, cached) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    assert!(!cached.contains("\"yanked\":\"bad build\""));

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
    let h = authority_harness().await;
    let digest = Digest::of(b"wheel");
    mount_detail(&h.server, digest.as_str(), "http://x/flask-1.0-py3-none-any.whl", None).await;

    h.clock.store(2000, Ordering::Relaxed);
    let status = request(&h.state, "DELETE", "/root/pypi/flask/", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::OK);

    let (_, _, merged) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;
    assert!(!merged.contains("flask-1.0-py3-none-any.whl"));
    let (_, _, cached) = get(&h.state, "/pypi/simple/flask/", Some("application/json")).await;
    assert!(cached.contains("flask-1.0-py3-none-any.whl"));

    h.clock.store(3000, Ordering::Relaxed);
    let status = request(&h.state, "PUT", "/root/pypi/flask/restore", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, restored) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;
    assert!(restored.contains("flask-1.0-py3-none-any.whl"));
    assert_eq!(
        read_journal_entries(&h.state.serving.meta, 0, 10)
            .unwrap()
            .entries
            .into_iter()
            .map(|entry| (entry.action, entry.submitted_at_unix))
            .collect::<Vec<_>>(),
        [("hide".to_owned(), 2000), ("restore".to_owned(), 3000)]
    );
}
#[tokio::test]
async fn test_restore_returns_an_upstream_file_still_yanked() {
    let h = authority_harness().await;
    let digest = Digest::of(b"wheel");
    mount_detail(&h.server, digest.as_str(), "http://x/flask-1.0-py3-none-any.whl", None).await;
    let status = request(
        &h.state,
        "PUT",
        "/root/pypi/flask/1.0/yank?reason=CVE-2026-1234",
        Some(&upload_auth()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let status = request(&h.state, "DELETE", "/root/pypi/flask/", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::OK);
    let status = request(&h.state, "PUT", "/root/pypi/flask/restore", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::OK);

    let (_, _, json) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;
    assert!(json.contains(r#""yanked":"CVE-2026-1234""#), "{json}");
    let (_, _, html) = get(&h.state, "/root/pypi/simple/flask/", Some("text/html")).await;
    assert!(html.contains(r#"data-yanked="CVE-2026-1234""#), "{html}");
}
#[tokio::test]
async fn test_restore_returns_an_unyanked_upstream_file_unyanked() {
    let h = authority_harness().await;
    let digest = Digest::of(b"wheel");
    mount_detail(&h.server, digest.as_str(), "http://x/flask-1.0-py3-none-any.whl", None).await;

    let status = request(&h.state, "DELETE", "/root/pypi/flask/", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::OK);
    let status = request(&h.state, "PUT", "/root/pypi/flask/restore", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::OK);

    let (_, _, json) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;
    assert!(json.contains("flask-1.0-py3-none-any.whl"), "{json}");
    assert!(!json.contains(r#""yanked":true"#), "{json}");
    let (_, _, html) = get(&h.state, "/root/pypi/simple/flask/", Some("text/html")).await;
    assert!(!html.contains("data-yanked"), "{html}");
}
#[tokio::test]
async fn test_unyanking_a_deleted_upstream_file_leaves_it_hidden() {
    let h = authority_harness().await;
    let digest = Digest::of(b"wheel");
    mount_detail(&h.server, digest.as_str(), "http://x/flask-1.0-py3-none-any.whl", None).await;
    let status = request(&h.state, "PUT", "/root/pypi/flask/1.0/yank", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::OK);
    let status = request(&h.state, "DELETE", "/root/pypi/flask/", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::OK);

    let status = request(&h.state, "DELETE", "/root/pypi/flask/1.0/yank", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::OK);

    let (_, _, json) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;
    assert!(!json.contains("flask-1.0-py3-none-any.whl"), "{json}");
    let status = request(&h.state, "PUT", "/root/pypi/flask/restore", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, restored) = get(&h.state, "/root/pypi/simple/flask/", Some("application/json")).await;
    assert!(restored.contains("flask-1.0-py3-none-any.whl"), "{restored}");
    assert!(!restored.contains(r#""yanked":true"#), "{restored}");
}
#[tokio::test]
async fn test_delete_one_upstream_version_leaves_other() {
    let h = authority_harness().await;
    let digest = Digest::of(b"wheel");
    let json = format!(
        "{{\"meta\":{{\"api-version\":\"1.1\"}},\"name\":\"flask\",\"versions\":[\"1.0\",\"2.0\"],\
         \"files\":[{{\"filename\":\"flask-1.0-py3-none-any.whl\",\"size\":11,\"url\":\"http://x/a.whl\",\
         \"hashes\":{{\"sha256\":\"{digest}\"}}}},\
         {{\"filename\":\"flask-2.0-py3-none-any.whl\",\"size\":11,\"url\":\"http://x/b.whl\",\
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
    let h = authority_harness().await;
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
    // Immutability applies to uploads, not reversible upstream overrides.
    let status = request(&h.state, "DELETE", "/root/pypi/flask/", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::OK);
}
#[tokio::test]
async fn test_yank_overlay_with_uploaded_file_skips_override() {
    let h = authority_harness().await;
    Mock::given(method("GET"))
        .and(path("/simple/peryxpkg/"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&h.server)
        .await;
    upload_wheel(&h.state, "peryxpkg-1.0-py3-none-any.whl", &fixture_wheel()).await;

    assert_eq!(
        request(&h.state, "PUT", "/root/pypi/peryxpkg/1.0/yank", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (_, _, detail) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert!(detail.contains("\"yanked\":true"));

    let status = request(&h.state, "PUT", "/root/pypi/peryxpkg/1.0/yank", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    assert_eq!(
        request(&h.state, "DELETE", "/root/pypi/peryxpkg/1.0/yank", Some(&upload_auth())).await,
        StatusCode::OK
    );
}
#[tokio::test]
async fn test_versioned_delete_matches_upload_record_when_filename_lacks_version() {
    let h = authority_harness().await;

    put_local_file(&h.state, "peryxpkg.whl", b"payload", "9.9");
    assert_eq!(
        request(&h.state, "DELETE", "/hosted/peryxpkg/9.9/", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (status, ..) = get(&h.state, "/hosted/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_ambiguous_stored_sdist_keeps_its_release_through_promotion() {
    let h = authority_promotion_harness().await;
    let (filename, digest) = store_ambiguous_sdist(&h.state, "staging");
    upload_wheel_to(
        &h.state,
        "/staging/",
        "peryxpkg-1.0-py3-none-any.whl",
        "1.0",
        &fixture_wheel(),
    )
    .await;

    let stored = h
        .state
        .serving
        .meta
        .get_upload("staging", "peryxpkg", filename)
        .unwrap()
        .unwrap();
    assert!(!std::str::from_utf8(&stored).unwrap().contains("authoritative_version"));

    let source = crate::cache::local_detail(&h.state.serving, "staging", "peryxpkg")
        .unwrap()
        .unwrap();
    assert_eq!(
        source
            .files
            .iter()
            .find(|file| file.filename == filename)
            .and_then(|file| file.authoritative_version.as_deref()),
        Some("1.0-1")
    );
    assert_eq!(
        crate::ui_project_from_detail(&source)
            .files
            .iter()
            .find(|file| file.filename == filename)
            .and_then(|file| file.release.as_deref()),
        Some("1.0-1")
    );
    let (status, _, body) = get(&h.state, "/staging/peryxpkg/json", None).await;
    assert_eq!(status, StatusCode::OK);
    let legacy: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(legacy["releases"]["1.0-1"][0]["filename"], filename);
    assert_eq!(
        legacy["releases"]["1.0-1"][0]["url"],
        local_artifact_url("staging", digest.as_str(), filename)
    );
    assert!(legacy["releases"]["1.0"].as_array().is_some());
    let (status, _, body) = get(&h.state, "/staging/peryxpkg/1.0.post1/json", None).await;
    let legacy: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(legacy["urls"][0]["filename"], filename);
    assert_eq!(
        legacy["urls"][0]["url"],
        local_artifact_url("staging", digest.as_str(), filename)
    );

    assert_eq!(
        request(
            &h.state,
            "PUT",
            "/prod/peryxpkg/1.0.post1/promote?from=staging",
            Some(&upload_auth())
        )
        .await,
        StatusCode::OK
    );

    let target = crate::cache::local_detail(&h.state.serving, "prod", "peryxpkg")
        .unwrap()
        .unwrap();
    assert_eq!(target.files[0].authoritative_version.as_deref(), Some("1.0-1"));
    assert_eq!(
        crate::ui_project_from_detail(&target).files[0].release.as_deref(),
        Some("1.0-1")
    );
    let entries = h.state.serving.meta.list_upload_entries("prod", "peryxpkg").unwrap();
    let promoted: crate::upload::Uploaded = serde_json::from_slice(&entries[0].1).unwrap();
    assert_eq!(promoted.version, "1.0-1");
    let (status, _, body) = get(&h.state, "/prod/peryxpkg/json", None).await;
    let legacy: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        legacy["releases"]["1.0-1"][0]["url"],
        local_artifact_url("prod", digest.as_str(), filename)
    );
    let (status, _, body) = get(&h.state, "/prod/peryxpkg/1.0.post1/json", None).await;
    let legacy: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        legacy["urls"][0]["url"],
        local_artifact_url("prod", digest.as_str(), filename)
    );
}

#[tokio::test]
async fn test_release_mutations_target_the_stored_ambiguous_sdist_version() {
    let h = authority_harness().await;
    let (filename, _) = store_ambiguous_sdist(&h.state, "hosted");
    put_local_file(&h.state, "peryxpkg-1.0-py3-none-any.whl", b"current wheel", "1.0");

    assert_eq!(
        request(
            &h.state,
            "PUT",
            "/hosted/peryxpkg/1.0.post1/yank?reason=historical",
            Some(&upload_auth())
        )
        .await,
        StatusCode::OK
    );
    let (_, _, yanked) = get(&h.state, "/hosted/simple/peryxpkg/", Some("application/json")).await;
    let yanked: serde_json::Value = serde_json::from_str(&yanked).unwrap();
    let files = yanked["files"].as_array().unwrap();
    assert_eq!(
        files.iter().find(|file| file["filename"] == filename).unwrap()["yanked"],
        "historical"
    );
    assert_eq!(
        files
            .iter()
            .find(|file| file["filename"] == "peryxpkg-1.0-py3-none-any.whl")
            .unwrap()["yanked"],
        false
    );

    assert_eq!(
        request(&h.state, "DELETE", "/hosted/peryxpkg/1.0.post1/", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (_, _, remaining) = get(&h.state, "/hosted/simple/peryxpkg/", Some("application/json")).await;
    assert!(remaining.contains("peryxpkg-1.0-py3-none-any.whl"), "{remaining}");
    assert!(!remaining.contains(filename), "{remaining}");
}
#[tokio::test]
async fn test_versioned_delete_removes_parsable_and_opaque_filenames() {
    let h = authority_harness().await;
    put_local_file(&h.state, "peryxpkg-1.0-py3-none-any.whl", b"normal", "1.0");
    put_local_file(&h.state, "peryxpkg-build.whl", b"opaque", "1.0");
    assert_eq!(
        request(&h.state, "DELETE", "/hosted/peryxpkg/1.0/", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (status, ..) = get(&h.state, "/hosted/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
#[tokio::test]
async fn test_versioned_delete_fallback_skips_other_versions() {
    let h = authority_harness().await;

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
async fn test_versioned_delete_fallback_matches_upload_by_pep440_equality() {
    let h = authority_harness().await;
    // Record fallback retains PEP 440 version equality.
    put_local_file(&h.state, "peryxpkg.whl", b"payload", "1.0");
    assert_eq!(
        request(&h.state, "DELETE", "/hosted/peryxpkg/1.0.0/", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (status, ..) = get(&h.state, "/hosted/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
#[tokio::test]
async fn test_versioned_delete_fallback_on_non_volatile_is_forbidden() {
    let h = harness_with(true, false).await;
    // Record fallback must preserve non-volatile upload protection.
    put_local_file(&h.state, "python-dateutil.tar.gz", b"payload", "2.8.2");
    let (status, body) = request_response(&h.state, "DELETE", "/hosted/peryxpkg/2.8.2/", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body, "file removal: index is not volatile; delete is disabled");
    let (_, _, detail) = get(&h.state, "/hosted/simple/peryxpkg/", Some("application/json")).await;
    assert!(detail.contains("python-dateutil.tar.gz"));
}
#[tokio::test]
async fn test_restore_skips_yanked_overrides_and_other_versions() {
    let h = authority_harness().await;
    h.state
        .serving
        .meta
        .set_override(
            true,
            "hosted",
            "flask",
            "flask-1.0-py3-none-any.whl",
            crate::store::OverrideMutation::Yanked(&Yanked::Yes),
            0,
        )
        .unwrap();
    h.state
        .serving
        .meta
        .set_override(
            true,
            "hosted",
            "flask",
            "flask-2.0-py3-none-any.whl",
            crate::store::OverrideMutation::Hidden(true),
            0,
        )
        .unwrap();

    let status = request(&h.state, "PUT", "/root/pypi/flask/1.0/restore", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let status = request(&h.state, "PUT", "/root/pypi/flask/2.0/restore", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::OK);
}
#[tokio::test]
async fn test_yank_with_corrupt_record_is_server_error() {
    let h = authority_harness().await;
    h.state
        .serving
        .meta
        .put_upload("hosted", "peryxpkg", "peryxpkg-1.0.whl", b"{ not json")
        .unwrap();
    let status = request(&h.state, "PUT", "/hosted/peryxpkg/1.0/yank", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn test_yank_reports_an_override_store_error() {
    let (_dir, state) = state_with_broken_journal();
    let (status, body) = request_response(&state, "PUT", "/root/pypi/flask/1.0/yank", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        body,
        "file removal: metadata store error: journal has key type &str and value type &[u8]"
    );
}

#[tokio::test]
async fn test_delete_reports_an_override_store_error() {
    let (_dir, state) = state_with_broken_journal();
    let (status, body) = request_response(&state, "DELETE", "/root/pypi/flask/1.0/", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        body,
        "file removal: metadata store error: journal has key type &str and value type &[u8]"
    );
}

fn state_with_broken_journal() -> (tempfile::TempDir, Arc<AppState>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let database = redb::Database::create(&path).unwrap();
    let txn = database.begin_write().unwrap();
    txn.open_table(redb::TableDefinition::<&str, &[u8]>::new("journal"))
        .unwrap();
    txn.commit().unwrap();
    drop(database);

    let meta = MetaStore::open_existing(path).unwrap();
    let digest = Digest::of(b"wheel");
    meta.put_index(
        "pypi/flask",
        &CachedIndex {
            source: None,
            last_modified: None,
            etag: None,
            last_serial: None,
            fetched_at_unix: 1_000,
            content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
            fresh_secs: None,
            body: detail_json(digest.as_str(), "https://files.example/flask-1.0-py3-none-any.whl").into_bytes(),
        },
    )
    .unwrap();
    let upstream = UpstreamClient::new("http://127.0.0.1:0/simple/").unwrap();
    let indexes = vec![
        Index {
            name: "pypi".to_owned(),
            route: "pypi".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Cached {
                client: upstream,
                offline: false,
            },
            policy: Policy::default(),
            acl: peryx_identity::IndexAcl::default(),
        },
        Index {
            name: "hosted".to_owned(),
            route: "hosted".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Hosted { volatile: true },
            policy: Policy::default(),
            acl: crate::tests::writer_acl("s3cret"),
        },
        Index {
            name: "root-pypi".to_owned(),
            route: "root/pypi".to_owned(),
            ecosystem: crate::ECOSYSTEM,
            kind: IndexKind::Virtual {
                layers: vec![1, 0],
                write_target: Some(1),
            },
            policy: Policy::default(),
            acl: peryx_identity::IndexAcl::default(),
        },
    ];
    let state = AppState::with_clock(
        meta,
        BlobStorage::filesystem(dir.path().join("blobs")),
        60,
        indexes,
        Arc::new(|| 1_000),
    );
    (dir, crate::tests::wired_distributed(state))
}

#[tokio::test]
async fn test_delete_project_named_yank() {
    // `yank` is also a legal PEP 503 project name.
    let h = authority_harness().await;
    put_local_project(&h.state, "yank", "yank-1.0-py3-none-any.whl", b"payload", "1.0");
    assert_eq!(
        request(&h.state, "DELETE", "/hosted/yank/", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (status, ..) = get(&h.state, "/hosted/simple/yank/", Some("application/json")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
#[tokio::test]
async fn test_yank_project_named_yank() {
    let h = authority_harness().await;
    put_local_project(&h.state, "yank", "yank-1.0-py3-none-any.whl", b"payload", "1.0");
    assert_eq!(
        request(&h.state, "PUT", "/hosted/yank/yank", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (_, _, detail) = get(&h.state, "/hosted/simple/yank/", Some("application/json")).await;
    assert!(detail.contains("\"yanked\":true"));
}
#[tokio::test]
async fn test_delete_project_named_restore() {
    let h = authority_harness().await;
    put_local_project(&h.state, "restore", "restore-1.0-py3-none-any.whl", b"payload", "1.0");
    assert_eq!(
        request(&h.state, "DELETE", "/hosted/restore/", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (status, ..) = get(&h.state, "/hosted/simple/restore/", Some("application/json")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
#[tokio::test]
async fn test_soft_delete_hides_file_but_keeps_blob_and_trash_metadata() {
    let h = authority_harness().await;
    upload_peryxpkg(&h.state, "/hosted/", &fixture_wheel()).await;
    let before = blob_count(&h.state);

    let status = request(
        &h.state,
        "DELETE",
        "/hosted/peryxpkg/1.0/?reason=bad+build",
        Some(&upload_auth()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (served, ..) = get(&h.state, "/hosted/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(served, StatusCode::NOT_FOUND);
    assert_eq!(blob_count(&h.state), before);

    let entries = h.state.serving.meta.list_upload_entries("hosted", "peryxpkg").unwrap();
    let (_, bytes) = entries.first().expect("the soft-deleted record is kept");
    let record: crate::upload::Uploaded = serde_json::from_slice(bytes).unwrap();
    let trash = record.trashed.expect("the record carries trash metadata");
    assert_eq!(trash.deleted_at_unix, 1000);
    assert_eq!(trash.reason.as_deref(), Some("bad build"));
    assert_eq!(trash.actor.as_deref(), Some("uploader"));
}
#[tokio::test]
async fn test_soft_delete_then_restore_serves_the_file_again() {
    let h = authority_harness().await;
    upload_peryxpkg(&h.state, "/hosted/", &fixture_wheel()).await;

    assert_eq!(
        request(&h.state, "DELETE", "/hosted/peryxpkg/1.0/", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (gone, ..) = get(&h.state, "/hosted/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(gone, StatusCode::NOT_FOUND);

    assert_eq!(
        request(&h.state, "PUT", "/hosted/peryxpkg/1.0/restore", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (back, _, body) = get(&h.state, "/hosted/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(back, StatusCode::OK);
    assert!(body.contains("peryxpkg-1.0"));
}

#[tokio::test]
async fn test_restore_rolls_back_every_file_on_a_release_import_conflict() {
    let h = authority_harness().await;
    for (build, metadata, imports) in [
        (
            1,
            b"Metadata-Version: 2.5\nName: peryxpkg\nVersion: 1.0\nRequires-Python: >=3.8\nImport-Name: peryxpkg\n"
                .as_slice(),
            exclusive_imports("peryxpkg"),
        ),
        (
            2,
            b"Metadata-Version: 2.5\nName: peryxpkg\nVersion: 1.0\nRequires-Python: >=3.8\nImport-Namespace: peryxpkg\n"
                .as_slice(),
            shared_imports("peryxpkg"),
        ),
    ] {
        let filename = format!("peryxpkg-1.0-{build}-py3-none-any.whl");
        store_historical_wheel(
            &h.state,
            "hosted",
            &filename,
            &fixture_wheel_with_build_and_metadata(&build.to_string(), metadata),
            imports,
            Some(TrashInfo {
                deleted_at_unix: 1000,
                actor: None,
                reason: None,
            }),
        );
    }
    let serial = h.state.serving.meta.current_serial().unwrap();

    let (status, body) = request_response(&h.state, "PUT", "/hosted/peryxpkg/1.0/restore", Some(&upload_auth())).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("exclusive and shared"), "{body}");
    assert_eq!(h.state.serving.meta.current_serial().unwrap(), serial);
    let records = h.state.serving.meta.list_upload_entries("hosted", "peryxpkg").unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(
        records
            .iter()
            .filter(|(_, bytes)| serde_json::from_slice::<crate::upload::Uploaded>(bytes)
                .unwrap()
                .trashed
                .is_some())
            .count(),
        records.len()
    );
}

#[tokio::test]
async fn test_soft_delete_twice_is_idempotent() {
    let h = authority_harness().await;
    upload_peryxpkg(&h.state, "/hosted/", &fixture_wheel()).await;
    assert_eq!(
        request(&h.state, "DELETE", "/hosted/peryxpkg/1.0/", Some(&upload_auth())).await,
        StatusCode::OK
    );

    assert_eq!(
        request(&h.state, "DELETE", "/hosted/peryxpkg/1.0/", Some(&upload_auth())).await,
        StatusCode::NOT_FOUND
    );
}
#[tokio::test]
async fn test_restore_one_version_leaves_the_other_trashed() {
    let h = authority_harness().await;
    upload_version(&h.state, "/hosted/", "1.0").await;
    upload_version(&h.state, "/hosted/", "2.0").await;

    assert_eq!(
        request(&h.state, "DELETE", "/hosted/peryxpkg/", Some(&upload_auth())).await,
        StatusCode::OK
    );
    assert_eq!(
        request(&h.state, "PUT", "/hosted/peryxpkg/1.0/restore", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (_, _, body) = get(&h.state, "/hosted/simple/peryxpkg/", Some("application/json")).await;
    assert!(body.contains("peryxpkg-1.0"));
    assert!(!body.contains("peryxpkg-2.0"));
}
fn stored_upload(state: &AppState, filename: &str) -> crate::upload::Uploaded {
    serde_json::from_slice(
        &state
            .serving
            .meta
            .get_upload("hosted", "peryxpkg", filename)
            .unwrap()
            .unwrap(),
    )
    .unwrap()
}

/// Drop a stored upload's import classification, as a record written before peryx classified imports.
fn unclassify(state: &AppState, filename: &str) {
    let mut uploaded = stored_upload(state, filename);
    uploaded.imports = None;
    state
        .serving
        .meta
        .put_upload("hosted", "peryxpkg", filename, crate::to_json(&uploaded).as_bytes())
        .unwrap();
}

/// A file coming back from the trash rejoins its release, so a restore classifies it first, exactly as
/// an upload into that release would.
#[tokio::test]
async fn test_restore_classifies_a_legacy_file_it_brings_back() {
    let h = authority_harness().await;
    upload_version(&h.state, "/hosted/", "1.0").await;
    assert_eq!(
        request(&h.state, "DELETE", "/hosted/peryxpkg/1.0/", Some(&upload_auth())).await,
        StatusCode::OK
    );
    unclassify(&h.state, "peryxpkg-1.0-py3-none-any.whl");

    assert_eq!(
        request(&h.state, "PUT", "/hosted/peryxpkg/1.0/restore", Some(&upload_auth())).await,
        StatusCode::OK
    );

    assert!(
        stored_upload(&h.state, "peryxpkg-1.0-py3-none-any.whl")
            .imports
            .is_some()
    );
}

/// A restore classifies only the releases it brings files back into; a live release it leaves alone
/// keeps its legacy records for the next upload into it to settle.
#[tokio::test]
async fn test_restore_classifies_only_the_releases_it_restores() {
    let h = authority_harness().await;
    upload_version(&h.state, "/hosted/", "1.0").await;
    upload_version(&h.state, "/hosted/", "2.0").await;
    assert_eq!(
        request(&h.state, "DELETE", "/hosted/peryxpkg/1.0/", Some(&upload_auth())).await,
        StatusCode::OK
    );
    unclassify(&h.state, "peryxpkg-2.0-py3-none-any.whl");

    assert_eq!(
        request(&h.state, "PUT", "/hosted/peryxpkg/restore", Some(&upload_auth())).await,
        StatusCode::OK
    );

    assert!(
        stored_upload(&h.state, "peryxpkg-2.0-py3-none-any.whl")
            .imports
            .is_none()
    );
}

/// A restore that brings nothing back has nothing new to serve, so the render a prior read cached
/// stays current.
#[tokio::test]
async fn test_restore_that_restores_nothing_keeps_the_cached_render() {
    let h = authority_harness().await;
    upload_version(&h.state, "/hosted/", "1.0").await;
    let key = h
        .state
        .serving
        .representation_key("hosted", "peryxpkg", crate::cache::SIMPLE_JSON);

    assert_eq!(
        crate::cache::restore_files(&h.state.serving, "hosted", "peryxpkg", None)
            .await
            .unwrap(),
        0
    );

    let current = h
        .state
        .serving
        .representation_key("hosted", "peryxpkg", crate::cache::SIMPLE_JSON);
    assert_eq!(current, key);
}

#[tokio::test]
async fn test_restore_with_only_live_uploads_is_not_found() {
    let h = authority_harness().await;
    upload_peryxpkg(&h.state, "/hosted/", &fixture_wheel()).await;

    assert_eq!(
        request(&h.state, "PUT", "/hosted/peryxpkg/1.0/restore", Some(&upload_auth())).await,
        StatusCode::NOT_FOUND
    );
}
#[tokio::test]
async fn test_delete_project_named_promote() {
    let h = authority_harness().await;
    put_local_project(&h.state, "promote", "promote-1.0-py3-none-any.whl", b"payload", "1.0");
    assert_eq!(
        request(&h.state, "DELETE", "/hosted/promote/", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (status, ..) = get(&h.state, "/hosted/simple/promote/", Some("application/json")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_yank_applies_at_the_current_authority_epoch() {
    let h = authority_harness().await;
    upload_peryxpkg(&h.state, "/root/pypi/", &fixture_wheel()).await;

    install_authority(
        &h.state,
        AuthorityDouble {
            committed: 5,
            current: 5,
            ..AuthorityDouble::default()
        },
    );
    assert_eq!(
        request(&h.state, "PUT", "/root/pypi/peryxpkg/1.0/yank", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (_, _, page) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert!(page.contains("\"yanked\":true"));
}

#[tokio::test]
async fn test_yank_under_a_superseded_epoch_conflicts_and_writes_nothing() {
    let h = authority_harness().await;
    upload_peryxpkg(&h.state, "/root/pypi/", &fixture_wheel()).await;

    install_authority(
        &h.state,
        AuthorityDouble {
            committed: 5,
            current: 6,
            ..AuthorityDouble::default()
        },
    );
    let (status, body) = request_response(&h.state, "PUT", "/root/pypi/peryxpkg/1.0/yank", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (_, _, page) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert!(!page.contains("\"yanked\":true"));
    assert_no_topology(&body);
}

#[tokio::test]
async fn test_yank_resumes_migration_after_authority_changes_between_pages() {
    let h = authority_harness().await;
    for position in 0..129 {
        let filename = format!("peryxpkg-1.0-{position:03}-py3-none-any.whl");
        let uploaded = upload_record(
            &filename,
            "1.0",
            format!("https://files.invalid/{filename}"),
            BTreeMap::new(),
            None,
        );
        h.state
            .serving
            .meta
            .put_driver_value(
                &format!("pypi\0u\0hosted/peryxpkg/{filename}"),
                crate::to_json(&uploaded).as_bytes(),
            )
            .unwrap();
    }
    h.state
        .serving
        .meta
        .put_project("hosted", "peryxpkg", "PeryxPkg")
        .unwrap();
    let begin_calls = Arc::new(AtomicUsize::new(0));
    install_authority(
        &h.state,
        AuthorityDouble {
            committed: 5,
            current: 5,
            begin_calls: Arc::clone(&begin_calls),
            write_limit: 1,
            ..AuthorityDouble::default()
        },
    );

    assert_eq!(
        request(&h.state, "PUT", "/root/pypi/peryxpkg/1.0/yank", Some(&upload_auth())).await,
        StatusCode::CONFLICT
    );
    assert_eq!(begin_calls.load(Ordering::Relaxed), 2);
    assert_eq!(h.state.serving.meta.current_serial().unwrap(), 1);

    install_authority(
        &h.state,
        AuthorityDouble {
            committed: 5,
            current: 5,
            ..AuthorityDouble::default()
        },
    );
    assert_eq!(
        request(&h.state, "PUT", "/root/pypi/peryxpkg/1.0/yank", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (_, _, page) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(page.matches("\"yanked\":true").count(), 129);
}

#[tokio::test]
async fn test_delete_at_the_current_authority_epoch_applies() {
    let h = authority_harness().await;
    upload_peryxpkg(&h.state, "/root/pypi/", &fixture_wheel()).await;
    install_authority(
        &h.state,
        AuthorityDouble {
            committed: 9,
            current: 9,
            ..AuthorityDouble::default()
        },
    );
    assert_eq!(
        request(&h.state, "DELETE", "/root/pypi/peryxpkg/", Some(&upload_auth())).await,
        StatusCode::OK
    );
    let (status, ..) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_delete_under_a_superseded_epoch_conflicts_and_keeps_the_file() {
    let h = authority_harness().await;
    upload_peryxpkg(&h.state, "/root/pypi/", &fixture_wheel()).await;
    install_authority(
        &h.state,
        AuthorityDouble {
            committed: 5,
            current: 6,
            ..AuthorityDouble::default()
        },
    );
    let (status, body) = request_response(&h.state, "DELETE", "/root/pypi/peryxpkg/", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, ..) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(status, StatusCode::OK);
    assert_no_topology(&body);
}

#[tokio::test]
async fn test_restore_under_a_superseded_epoch_conflicts() {
    let h = authority_harness().await;
    upload_peryxpkg(&h.state, "/root/pypi/", &fixture_wheel()).await;
    assert_eq!(
        request(&h.state, "DELETE", "/root/pypi/peryxpkg/", Some(&upload_auth())).await,
        StatusCode::OK
    );
    install_authority(
        &h.state,
        AuthorityDouble {
            committed: 5,
            current: 6,
            ..AuthorityDouble::default()
        },
    );
    let (status, body) = request_response(&h.state, "PUT", "/root/pypi/peryxpkg/restore", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, ..) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_no_topology(&body);
}

fn assert_no_topology(body: &str) {
    let lowered = body.to_ascii_lowercase();
    for leaked in [
        "leader",
        "voter",
        "datacenter",
        "://",
        "127.0.0.1",
        ".internal",
        "node ",
    ] {
        assert!(
            !lowered.contains(leaked),
            "stale-epoch response leaked {leaked:?}: {body}"
        );
    }
    assert!(
        lowered.contains("retry"),
        "stale-epoch response should guide a retry: {body}"
    );
}

#[tokio::test]
async fn test_deleting_every_file_takes_the_project_out_of_the_root_listing() {
    let h = authority_harness().await;
    upload_peryxpkg(&h.state, "/root/pypi/", &fixture_wheel()).await;

    assert_eq!(
        request(&h.state, "DELETE", "/root/pypi/peryxpkg/", Some(&upload_auth())).await,
        StatusCode::OK
    );

    let (detail_status, ..) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    let (list_status, _, list) = get(&h.state, "/root/pypi/simple/", Some("application/json")).await;

    assert_eq!((detail_status, list_status), (StatusCode::NOT_FOUND, StatusCode::OK));
    assert!(
        !list.contains("peryxpkg"),
        "the root index still advertises a project whose detail page is gone: {list}"
    );
}

#[tokio::test]
async fn test_restoring_a_trashed_project_returns_it_with_its_published_spelling() {
    let h = authority_harness().await;
    upload_peryxpkg(&h.state, "/root/pypi/", &fixture_wheel()).await;
    let (_, _, published) = get(&h.state, "/root/pypi/simple/", Some("application/json")).await;
    assert_eq!(
        request(&h.state, "DELETE", "/root/pypi/peryxpkg/", Some(&upload_auth())).await,
        StatusCode::OK
    );

    assert_eq!(
        request(&h.state, "PUT", "/root/pypi/peryxpkg/restore", Some(&upload_auth())).await,
        StatusCode::OK
    );

    let (status, _, restored) = get(&h.state, "/root/pypi/simple/", Some("application/json")).await;
    assert_eq!((status, restored), (StatusCode::OK, published));
}

#[tokio::test]
async fn test_deleting_one_file_of_a_multi_file_project_keeps_the_project_listed() {
    let h = authority_harness().await;
    upload_peryxpkg(&h.state, "/root/pypi/", &fixture_wheel()).await;
    upload_version(&h.state, "/root/pypi/", "2.0").await;

    assert_eq!(
        request(&h.state, "DELETE", "/root/pypi/peryxpkg/1.0", Some(&upload_auth())).await,
        StatusCode::OK
    );

    let (detail_status, ..) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    let (_, _, list) = get(&h.state, "/root/pypi/simple/", Some("application/json")).await;
    assert_eq!(detail_status, StatusCode::OK);
    assert!(
        list.contains("peryxpkg"),
        "a project that still serves a file stays listed: {list}"
    );
}

#[tokio::test]
async fn test_a_token_scoped_to_another_project_cannot_delete_this_one() {
    let h = authority_harness().await;
    // The path carries no trailing slash, so which project the token is being spent on has to come from
    // the segment rather than from an empty one the split leaves behind.
    upload_peryxpkg(&h.state, "/hosted/", &fixture_wheel()).await;

    let status = request(&h.state, "DELETE", "/hosted/peryxpkg/1.0", Some(&narrow_auth())).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    let (served, ..) = get(&h.state, "/hosted/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(served, StatusCode::OK, "the refused delete left the project served");
}

#[tokio::test]
async fn test_a_delete_carrying_no_reason_records_none() {
    let h = authority_harness().await;
    upload_peryxpkg(&h.state, "/hosted/", &fixture_wheel()).await;

    let status = request(
        &h.state,
        "DELETE",
        "/hosted/peryxpkg/1.0/?ignored=1",
        Some(&upload_auth()),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    let entries = h.state.serving.meta.list_upload_entries("hosted", "peryxpkg").unwrap();
    let (_, bytes) = entries.first().expect("the soft-deleted record is kept");
    let record: crate::upload::Uploaded = serde_json::from_slice(bytes).unwrap();
    let trash = record.trashed.expect("the record carries trash metadata");
    assert_eq!(trash.reason, None, "only a reason parameter supplies the reason");
}

/// The narrow token is granted `other` and nothing else, so it deleting `other` is the one request
/// that tells the project taken from the path apart from any other spelling of it.
#[tokio::test]
async fn test_a_token_scoped_to_this_project_can_delete_it() {
    let h = authority_harness().await;
    let wheel = fixture_wheel_for_project("other", "1.0");
    let fields = [
        (":action", "file_upload"),
        ("name", "other"),
        ("version", "1.0"),
        ("filetype", "bdist_wheel"),
    ];
    let (content_type, body) = multipart_body(&fields, Some(("other-1.0-py3-none-any.whl", &wheel)));
    assert_eq!(
        post_upload(&h.state, "/hosted/", Some(&narrow_auth()), &content_type, body).await,
        StatusCode::OK
    );

    let status = request(&h.state, "DELETE", "/hosted/other/1.0", Some(&narrow_auth())).await;

    assert_eq!(status, StatusCode::OK);
    let (served, ..) = get(&h.state, "/hosted/simple/other/", Some("application/json")).await;
    assert_eq!(
        served,
        StatusCode::NOT_FOUND,
        "the token spent on its own project removed it"
    );
}
