use super::support::*;
use crate::store::read_journal_entries;

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
    let h = authority_promotion_harness().await;
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
    let h = authority_promotion_harness().await;
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

    h.clock.store(2000, Ordering::Relaxed);
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
    let entry = read_journal_entries(&h.state.serving.meta, 0, 10)
        .unwrap()
        .entries
        .pop()
        .unwrap();
    assert_eq!(
        (
            entry.action.as_str(),
            entry.version.as_deref(),
            entry.filename.as_deref(),
            entry.submitted_at_unix,
        ),
        ("add-file", Some("1.0"), Some("peryxpkg-1.0-py3-none-any.whl"), 2000,)
    );
}

#[tokio::test]
async fn test_promote_journals_the_artifact_reference() {
    let h = authority_promotion_harness().await;
    let wheel = fixture_wheel();
    let digest = upload_wheel_to(&h.state, "/staging/", "peryxpkg-1.0-py3-none-any.whl", "1.0", &wheel).await;
    let serial = h.state.serving.meta.current_serial().unwrap();

    let status = request(
        &h.state,
        "PUT",
        "/prod/peryxpkg/1.0/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        h.state.serving.meta.journal_after(serial, 1).unwrap()[0].blobs,
        vec![peryx_storage::meta::DriverBlobReference {
            sha256: digest.as_str().to_owned(),
            size: wheel.len() as u64,
        }]
    );
}

#[tokio::test]
async fn test_promote_matches_source_by_pep440_equality() {
    let h = authority_promotion_harness().await;
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
    let h = authority_promotion_harness().await;
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
    let h = authority_promotion_harness().await;
    let filename = "peryxpkg-1.0-py3-none-any.whl";
    let uploaded = upload_record(
        filename,
        "1.0",
        "https://example.test/pkg.whl".to_owned(),
        BTreeMap::new(),
        Some(4),
    );
    h.state
        .serving
        .meta
        .put_upload("staging", "peryxpkg", filename, &to_json(&uploaded).into_bytes())
        .unwrap();
    h.state
        .serving
        .meta
        .put_project("staging", "peryxpkg", "peryxpkg")
        .unwrap();
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
async fn test_promote_uses_resource_key_when_source_display_is_missing() {
    let h = authority_promotion_harness().await;
    let filename = "peryxpkg-1.0-py3-none-any.whl";
    let digest = Digest::of(b"wheel");
    let uploaded = upload_record(
        filename,
        "1.0",
        local_artifact_url("staging", digest.as_str(), filename),
        BTreeMap::from([("sha256".to_owned(), digest.as_str().to_owned())]),
        Some(5),
    );
    h.state
        .serving
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
    let h = authority_promotion_harness().await;

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
async fn test_promote_reports_no_live_source_release() {
    let h = authority_promotion_harness().await;
    trash_staging_release(&h.state).await;
    let serial = h.state.serving.meta.current_serial().unwrap();

    let response = request_response(
        &h.state,
        "PUT",
        "/prod/peryxpkg/1.0/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;
    let target_status = get(&h.state, "/prod/simple/peryxpkg/", Some("application/json"))
        .await
        .0;

    assert_eq!(
        (response, target_status, h.state.serving.meta.current_serial().unwrap(),),
        (
            (
                StatusCode::NOT_FOUND,
                "promotion: no uploaded files on source \"staging\" match project \"peryxpkg\" version \"1.0\""
                    .to_owned(),
            ),
            StatusCode::NOT_FOUND,
            serial,
        )
    );
}

#[tokio::test]
async fn test_promote_copies_only_live_source_files() {
    let h = authority_promotion_harness().await;
    trash_staging_release(&h.state).await;
    assert_eq!(upload_version(&h.state, "/staging/", "1.0.0").await, StatusCode::OK);
    let serial = h.state.serving.meta.current_serial().unwrap();
    let logs = LogCapture::default();
    let _guard = logs.install();

    let response = request_response(
        &h.state,
        "PUT",
        "/prod/peryxpkg/1.0/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;
    let (_, _, detail) = get(&h.state, "/prod/simple/peryxpkg/", Some("application/json")).await;
    let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
    let journal = read_journal_entries(&h.state.serving.meta, serial, 10)
        .unwrap()
        .entries
        .into_iter()
        .map(|entry| (entry.action, entry.version, entry.filename))
        .collect::<Vec<_>>();
    let event = logs
        .security_events()
        .into_iter()
        .find(|event| field(event, "action") == Some("promote"))
        .unwrap();

    assert_eq!(
        (
            response,
            detail["files"]
                .as_array()
                .unwrap()
                .iter()
                .map(|file| file["filename"].as_str().unwrap())
                .collect::<Vec<_>>(),
            journal,
            field(&event, "result"),
            event["fields"]["count"].as_u64(),
        ),
        (
            (StatusCode::OK, "promoted 1 file(s)".to_owned()),
            vec!["peryxpkg-1.0.0-py3-none-any.whl"],
            vec![(
                "add-file".to_owned(),
                Some("1.0.0".to_owned()),
                Some("peryxpkg-1.0.0-py3-none-any.whl".to_owned()),
            )],
            Some("success"),
            Some(1),
        )
    );
}

async fn trash_staging_release(state: &Arc<AppState>) {
    assert_eq!(upload_version(state, "/staging/", "1.0").await, StatusCode::OK);
    assert_eq!(
        request(state, "DELETE", "/staging/peryxpkg/1.0/", Some(&upload_auth()),).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn test_promote_conflicts_on_target_filename_with_different_bytes() {
    let h = authority_promotion_harness().await;
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
    let h = authority_promotion_harness().await;
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
        let h = authority_promotion_harness().await;
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

#[tokio::test]
async fn test_promote_at_the_current_authority_epoch_applies() {
    let h = authority_promotion_harness().await;
    let wheel = fixture_wheel();
    upload_wheel_to(&h.state, "/staging/", "peryxpkg-1.0-py3-none-any.whl", "1.0", &wheel).await;
    install_authority(
        &h.state,
        AuthorityDouble {
            committed: 4,
            current: 4,
            ..AuthorityDouble::default()
        },
    );
    let (status, body) = request_response(
        &h.state,
        "PUT",
        "/prod/peryxpkg/1.0/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "promoted 1 file(s)");
}

#[tokio::test]
async fn test_promote_under_a_superseded_epoch_conflicts_and_promotes_nothing() {
    let h = authority_promotion_harness().await;
    let wheel = fixture_wheel();
    upload_wheel_to(&h.state, "/staging/", "peryxpkg-1.0-py3-none-any.whl", "1.0", &wheel).await;
    // The target authority advanced past the epoch this promotion leased, so it is fenced.
    install_authority(
        &h.state,
        AuthorityDouble {
            committed: 5,
            current: 6,
            ..AuthorityDouble::default()
        },
    );
    let (status, body) = request_response(
        &h.state,
        "PUT",
        "/prod/peryxpkg/1.0/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, ..) = get(&h.state, "/prod/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let lowered = body.to_ascii_lowercase();
    for leaked in ["leader", "voter", "datacenter", "://", ".internal"] {
        assert!(
            !lowered.contains(leaked),
            "stale-epoch response leaked {leaked:?}: {body}"
        );
    }
}

fn promoter_auth() -> String {
    format!("Basic {}", STANDARD.encode("__token__:pr0m0te"))
}

/// Reads the target listing as `promoter`, the only credential `prod` grants a read to. Anonymously
/// the closed index answers `401` to promoted and absent alike, which would not tell the two apart.
async fn promoted_listing(state: &Arc<AppState>) -> (StatusCode, String) {
    let auth = promoter_auth();
    get_with_headers(
        state,
        "/prod/simple/peryxpkg/",
        &[("accept", "application/json"), ("authorization", &auth)],
    )
    .await
}

async fn stage_peryxpkg(state: &Arc<AppState>) {
    upload_wheel_to(
        state,
        "/staging/",
        "peryxpkg-1.0-py3-none-any.whl",
        "1.0",
        &fixture_wheel(),
    )
    .await;
}

#[tokio::test]
async fn test_promote_denies_a_source_the_caller_cannot_read() {
    let h = private_promotion_harness().await;
    stage_peryxpkg(&h.state).await;

    let (status, body) = request_response(
        &h.state,
        "PUT",
        "/prod/peryxpkg/1.0/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;

    assert_eq!((status, body.as_str()), (StatusCode::NOT_FOUND, "not found"));
    assert_eq!(promoted_listing(&h.state).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_promote_accepts_a_source_the_caller_can_read() {
    let h = private_promotion_harness().await;
    stage_peryxpkg(&h.state).await;

    let (status, body) = request_response(
        &h.state,
        "PUT",
        "/prod/peryxpkg/1.0/promote?from=staging",
        Some(&promoter_auth()),
    )
    .await;

    assert_eq!((status, body.as_str()), (StatusCode::OK, "promoted 1 file(s)"));
    let (listing_status, listing) = promoted_listing(&h.state).await;
    let detail: serde_json::Value = serde_json::from_str(&listing).unwrap();
    assert_eq!(listing_status, StatusCode::OK);
    assert_eq!(detail["files"][0]["filename"], "peryxpkg-1.0-py3-none-any.whl");
}

/// The virtual routes share `staging` as their write target, so only the named route's own ACL can
/// account for the two outcomes.
#[rstest]
#[case::sealed_route_over_readable_layer("sealed", "pr0m0te", StatusCode::NOT_FOUND, "not found")]
#[case::open_route_over_sealed_layer("open", "s3cret", StatusCode::OK, "promoted 1 file(s)")]
#[tokio::test]
async fn test_promote_authorizes_the_named_virtual_route_not_its_write_target(
    #[case] source: &str,
    #[case] secret: &str,
    #[case] expected: StatusCode,
    #[case] message: &str,
) {
    let h = private_promotion_harness().await;
    stage_peryxpkg(&h.state).await;
    let auth = format!("Basic {}", STANDARD.encode(format!("__token__:{secret}")));

    let (status, body) = request_response(
        &h.state,
        "PUT",
        &format!("/prod/peryxpkg/1.0/promote?from={source}"),
        Some(&auth),
    )
    .await;

    assert_eq!((status, body.as_str()), (expected, message));
}

/// `internal` would answer `405 no hosted upload layer` and `staging` would promote for a reader, so
/// an unreadable source has to be refused before either can distinguish itself from a missing route.
#[rstest]
#[case::missing("absent")]
#[case::hidden_cache("internal")]
#[case::unreadable_hosted("staging")]
#[case::unreadable_virtual("sealed")]
#[tokio::test]
async fn test_promote_hides_which_unreadable_source_was_named(#[case] source: &str) {
    let h = private_promotion_harness().await;
    stage_peryxpkg(&h.state).await;

    let (status, body) = request_response(
        &h.state,
        "PUT",
        &format!("/prod/peryxpkg/1.0/promote?from={source}"),
        Some(&upload_auth()),
    )
    .await;

    assert_eq!((status, body.as_str()), (StatusCode::NOT_FOUND, "not found"));
}

#[tokio::test(flavor = "current_thread")]
async fn test_promote_logs_the_refused_source_read() {
    let h = private_promotion_harness().await;
    stage_peryxpkg(&h.state).await;
    let logs = LogCapture::default();
    let guard = logs.install();

    assert_eq!(
        request(
            &h.state,
            "PUT",
            "/prod/peryxpkg/1.0/promote?from=staging",
            Some(&upload_auth()),
        )
        .await,
        StatusCode::NOT_FOUND
    );

    drop(guard);
    assert!(!logs.text().contains("s3cret"));
    let denial = logs
        .security_events()
        .into_iter()
        .find(|event| field(event, "action") == Some("promote"))
        .unwrap();
    assert_eq!(
        (
            field(&denial, "result"),
            field(&denial, "reason"),
            field(&denial, "index"),
            field(&denial, "source_index"),
            field(&denial, "hosted_index"),
            field(&denial, "resource"),
            field(&denial, "group"),
        ),
        (
            Some("denied"),
            Some("source read denied"),
            Some("prod"),
            Some("staging"),
            Some("prod"),
            Some("peryxpkg"),
            Some("1.0"),
        )
    );
}

/// A wheel the target would refuse from a direct upload is refused from a promotion too. Promotion
/// copies a record into the target's namespace, so the target's rules decide what may land there.
#[tokio::test]
async fn test_promote_applies_the_target_upload_policy() {
    let h = promotion_harness_with_target_policy(policy(|_neutral, pypi| {
        pypi.block_wheel_pythons = vec!["py3".to_owned()];
    }))
    .await;
    let wheel = fixture_wheel();
    upload_wheel_to(&h.state, "/staging/", "peryxpkg-1.0-py3-none-any.whl", "1.0", &wheel).await;

    let (status, _) = request_response(
        &h.state,
        "PUT",
        "/prod/peryxpkg/1.0/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    let (listing, _) = request_response(&h.state, "GET", "/prod/simple/peryxpkg/", None).await;
    assert_eq!(listing, StatusCode::NOT_FOUND);
}

/// A release that would cross the target's project-byte limit publishes no file at all, rather than
/// the prefix that happened to fit.
#[tokio::test]
async fn test_promote_refuses_a_release_that_crosses_the_target_quota() {
    let h = promotion_harness_with_target_policy(policy(|neutral, _pypi| {
        neutral.max_resource_size_bytes = Some(16);
    }))
    .await;
    let wheel = fixture_wheel();
    upload_wheel_to(&h.state, "/staging/", "peryxpkg-1.0-py3-none-any.whl", "1.0", &wheel).await;

    let (status, _) = request_response(
        &h.state,
        "PUT",
        "/prod/peryxpkg/1.0/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    let (listing, _) = request_response(&h.state, "GET", "/prod/simple/peryxpkg/", None).await;
    assert_eq!(listing, StatusCode::NOT_FOUND);
}

/// A promoted release accounts for each file it publishes, so the target's usage matches the bytes it
/// now stores rather than one synthetic row for the release.
#[tokio::test]
async fn test_promote_accounts_for_every_promoted_file() {
    let h = promotion_harness_with_target_policy(policy(|neutral, _pypi| {
        neutral.max_resource_size_bytes = Some(1_000_000);
    }))
    .await;
    let first = fixture_wheel();
    let second = fixture_sdist();
    upload_wheel_to(&h.state, "/staging/", "peryxpkg-1.0-py3-none-any.whl", "1.0", &first).await;
    upload_sdist_to(&h.state, "/staging/", "peryxpkg-1.0.tar.gz", "1.0", &second).await;

    let (status, body) = request_response(
        &h.state,
        "PUT",
        "/prod/peryxpkg/1.0/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;

    assert_eq!((status, body.as_str()), (StatusCode::OK, "promoted 2 file(s)"));
    assert_eq!(
        h.state
            .serving
            .meta
            .quota_usage("prod")
            .unwrap()
            .accounted_bytes
            .committed,
        (first.len() + second.len()) as u64
    );
}

/// Re-promoting the same release publishes nothing the second time, so it accounts for nothing the
/// second time either. A skipped file that still consumed quota would drift the target's usage above
/// what it stores.
#[tokio::test]
async fn test_promote_charges_no_quota_for_a_file_it_skips() {
    let h = promotion_harness_with_target_policy(policy(|neutral, _pypi| {
        neutral.max_resource_size_bytes = Some(1_000_000);
    }))
    .await;
    let wheel = fixture_wheel();
    upload_wheel_to(&h.state, "/staging/", "peryxpkg-1.0-py3-none-any.whl", "1.0", &wheel).await;
    let auth = upload_auth();
    request_response(&h.state, "PUT", "/prod/peryxpkg/1.0/promote?from=staging", Some(&auth)).await;
    let after_first = h
        .state
        .serving
        .meta
        .quota_usage("prod")
        .unwrap()
        .accounted_bytes
        .committed;

    let (status, body) =
        request_response(&h.state, "PUT", "/prod/peryxpkg/1.0/promote?from=staging", Some(&auth)).await;

    assert_eq!((status, body.as_str()), (StatusCode::OK, "promoted 0 file(s)"));
    assert_eq!(
        h.state
            .serving
            .meta
            .quota_usage("prod")
            .unwrap()
            .accounted_bytes
            .committed,
        after_first
    );
}

/// A target that refuses one file of a release publishes none of it, and accounts for none of it. Half
/// a release is neither the state the caller asked for nor the one they had.
#[tokio::test]
async fn test_promote_leaves_no_allocation_when_one_file_is_refused() {
    let h = promotion_harness_with_target_policy(policy(|neutral, pypi| {
        neutral.max_resource_size_bytes = Some(1_000_000);
        pypi.block_package_types = vec![crate::policy::PackageType::Sdist];
    }))
    .await;
    upload_wheel_to(
        &h.state,
        "/staging/",
        "peryxpkg-1.0-py3-none-any.whl",
        "1.0",
        &fixture_wheel(),
    )
    .await;
    upload_sdist_to(&h.state, "/staging/", "peryxpkg-1.0.tar.gz", "1.0", &fixture_sdist()).await;

    let (status, _) = request_response(
        &h.state,
        "PUT",
        "/prod/peryxpkg/1.0/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        h.state
            .serving
            .meta
            .quota_usage("prod")
            .unwrap()
            .accounted_bytes
            .committed,
        0
    );
    let (listing, _) = request_response(&h.state, "GET", "/prod/simple/peryxpkg/", None).await;
    assert_eq!(listing, StatusCode::NOT_FOUND);
}

/// The target's attestation rule judges the bundle the source publication carries, so a promotion is
/// admitted on the same evidence the upload was. Reading the stored document is what makes that
/// possible: the record itself keeps no predicate types.
#[tokio::test]
async fn test_promote_reads_the_stored_bundle_for_the_target_attestation_rule() {
    let h = promotion_harness_with_target_policy(policy(|_neutral, pypi| {
        pypi.required_attestations = vec!["https://docs.pypi.org/attestations/publish/v1".to_owned()];
    }))
    .await;
    let wheel = fixture_wheel();
    let field = super::attestations::attestations_field(super::attestations::FILENAME, Digest::of(&wheel).as_str());
    assert_eq!(
        super::attestations::upload_with_attestations_to(&h.state, "/staging/", &wheel, &field).await,
        StatusCode::OK
    );

    let (status, body) = request_response(
        &h.state,
        "PUT",
        "/prod/peryxpkg/1.0/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;

    assert_eq!((status, body.as_str()), (StatusCode::OK, "promoted 1 file(s)"));
}

/// A record written without a size still names bytes peryx holds, so the blob answers for it and the
/// target accounts for the file rather than refusing it.
#[tokio::test]
async fn test_promote_recovers_a_missing_size_from_the_stored_blob() {
    let h = promotion_harness_with_target_policy(policy(|neutral, _pypi| {
        neutral.max_resource_size_bytes = Some(1_000_000);
    }))
    .await;
    let wheel = fixture_wheel();
    upload_wheel_to(&h.state, "/staging/", "peryxpkg-1.0-py3-none-any.whl", "1.0", &wheel).await;
    forget_recorded_size(&h, "peryxpkg-1.0-py3-none-any.whl");

    let (status, body) = request_response(
        &h.state,
        "PUT",
        "/prod/peryxpkg/1.0/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;

    assert_eq!((status, body.as_str()), (StatusCode::OK, "promoted 1 file(s)"));
    assert_eq!(
        h.state
            .serving
            .meta
            .quota_usage("prod")
            .unwrap()
            .accounted_bytes
            .committed,
        wheel.len() as u64
    );
}

/// A record whose size neither its row nor a stored blob can supply cannot be accounted for, so the
/// target refuses it rather than publishing outside the limit the operator set.
#[tokio::test]
async fn test_promote_refuses_a_file_whose_size_nothing_can_supply() {
    let h = promotion_harness_with_target_policy(policy(|neutral, _pypi| {
        neutral.max_resource_size_bytes = Some(1_000_000);
    }))
    .await;
    upload_wheel_to(
        &h.state,
        "/staging/",
        "peryxpkg-1.0-py3-none-any.whl",
        "1.0",
        &fixture_wheel(),
    )
    .await;
    forget_recorded_size(&h, "peryxpkg-1.0-py3-none-any.whl");
    rewrite_digest(&h, "peryxpkg-1.0-py3-none-any.whl", &"a".repeat(64));

    let (status, _) = request_response(
        &h.state,
        "PUT",
        "/prod/peryxpkg/1.0/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        h.state
            .serving
            .meta
            .quota_usage("prod")
            .unwrap()
            .accounted_bytes
            .committed,
        0
    );
}

/// A repository-wide byte limit refuses the file by name, so the reply says which limit answered
/// rather than reporting a project total that did not move.
#[tokio::test]
async fn test_promote_reports_the_repository_limit_that_refused_the_file() {
    let h = promotion_harness_with_target_policy(policy(|neutral, _pypi| {
        neutral.max_accounted_bytes = Some(8);
    }))
    .await;
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
        "/prod/peryxpkg/1.0/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body.contains("exceeds the target quota"), "{body}");
}

/// Drop the size a stored upload row records, leaving the blob as the only place its byte count lives.
fn forget_recorded_size(h: &Harness, filename: &str) {
    rewrite_upload_row(h, filename, |record| {
        record.as_object_mut().unwrap()["file"]
            .as_object_mut()
            .unwrap()
            .remove("size");
    });
}

/// Point a stored upload row at a digest that names no blob peryx can even look up.
fn rewrite_digest(h: &Harness, filename: &str, digest: &str) {
    rewrite_upload_row(h, filename, |record| {
        record.as_object_mut().unwrap()["file"].as_object_mut().unwrap()["hashes"]
            .as_object_mut()
            .unwrap()
            .insert("sha256".to_owned(), serde_json::json!(digest));
    });
}

fn rewrite_upload_row(h: &Harness, filename: &str, edit: impl FnOnce(&mut serde_json::Value)) {
    let raw = h
        .state
        .serving
        .meta
        .get_upload("staging", "peryxpkg", filename)
        .unwrap()
        .unwrap();
    let mut record: serde_json::Value = serde_json::from_slice(&raw).unwrap();
    edit(&mut record);
    h.state
        .serving
        .meta
        .put_upload("staging", "peryxpkg", filename, &to_json(&record).into_bytes())
        .unwrap();
}

/// A conflict aborts the whole transaction, allocations included. The target keeps the usage its own
/// upload left and gains nothing from the promotion it refused.
#[tokio::test]
async fn test_promote_leaves_no_allocation_when_a_filename_conflicts() {
    let h = promotion_harness_with_target_policy(policy(|neutral, _pypi| {
        neutral.max_resource_size_bytes = Some(1_000_000);
    }))
    .await;
    upload_wheel_to(
        &h.state,
        "/staging/",
        "peryxpkg-1.0-py3-none-any.whl",
        "1.0",
        &fixture_wheel(),
    )
    .await;
    let resident = fixture_wheel_with_body("1.0", b"VALUE = 2\n");
    upload_wheel_to(&h.state, "/prod/", "peryxpkg-1.0-py3-none-any.whl", "1.0", &resident).await;
    let before = h
        .state
        .serving
        .meta
        .quota_usage("prod")
        .unwrap()
        .accounted_bytes
        .committed;

    let (status, _) = request_response(
        &h.state,
        "PUT",
        "/prod/peryxpkg/1.0/promote?from=staging",
        Some(&upload_auth()),
    )
    .await;

    assert_eq!((status, before), (StatusCode::CONFLICT, resident.len() as u64));
    assert_eq!(
        h.state
            .serving
            .meta
            .quota_usage("prod")
            .unwrap()
            .accounted_bytes
            .committed,
        before
    );
}
