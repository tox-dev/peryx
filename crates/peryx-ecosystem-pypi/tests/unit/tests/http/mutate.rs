use super::support::*;
use crate::store::read_journal_entries;

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
        })
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
        "file removal: metadata store error: journal is of type Table<&str, &[u8]>"
    );
}

#[tokio::test]
async fn test_delete_reports_an_override_store_error() {
    let (_dir, state) = state_with_broken_journal();
    let (status, body) = request_response(&state, "DELETE", "/root/pypi/flask/1.0/", Some(&upload_auth())).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        body,
        "file removal: metadata store error: journal is of type Table<&str, &[u8]>"
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
