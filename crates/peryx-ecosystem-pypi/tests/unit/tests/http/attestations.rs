use super::support::*;
use crate::policy::AttestationMode;
use peryx_driver::serving::{BrowseDriver as _, BrowseRequest};

pub(super) const FILENAME: &str = "peryxpkg-1.0-py3-none-any.whl";
const PUBLISH_PREDICATE: &str = "https://docs.pypi.org/attestations/publish/v1";
const SLSA_PREDICATE: &str = "https://slsa.dev/provenance/v1";
const HOSTILE_PREDICATE: &str = "<script>alert('xss')</script>";

fn statement(name: &str, sha256: &str) -> String {
    STANDARD.encode(
        serde_json::json!({
            "_type": "https://in-toto.io/Statement/v1",
            "subject": [{"name": name, "digest": {"sha256": sha256}}],
            "predicateType": PUBLISH_PREDICATE,
            "predicate": {"note": HOSTILE_PREDICATE},
        })
        .to_string(),
    )
}

pub(super) fn attestations_field(name: &str, sha256: &str) -> String {
    signed_attestations_field(name, sha256, "YmFy")
}

/// The same attestation under a chosen signature, so two publishers can attest the same
/// distribution and be told apart by the bytes peryx stores.
fn signed_attestations_field(name: &str, sha256: &str, signature: &str) -> String {
    serde_json::json!([{
        "version": 1,
        "verification_material": {"certificate": "Zm9v", "transparency_entries": []},
        "envelope": {"statement": statement(name, sha256), "signature": signature},
    }])
    .to_string()
}

pub(super) async fn upload_with_attestations(state: &Arc<AppState>, wheel: &[u8], field: &str) -> StatusCode {
    upload_with_attestations_to(state, "/root/pypi/", wheel, field).await
}

async fn upload_with_attestations_to(state: &Arc<AppState>, route: &str, wheel: &[u8], field: &str) -> StatusCode {
    let fields = vec![
        (":action", "file_upload"),
        ("name", "peryxpkg"),
        ("version", "1.0"),
        ("filetype", "bdist_wheel"),
        ("attestations", field),
    ];
    let (content_type, body) = multipart_body(&fields, Some((FILENAME, wheel)));
    post_upload(state, route, Some(&upload_auth()), &content_type, body).await
}

fn provenance_uri(sha256: &str) -> String {
    format!("/root/pypi/files/{sha256}/{FILENAME}.provenance")
}

#[tokio::test]
async fn test_hosted_provenance_with_a_missing_blob_is_not_found() {
    let harness = harness().await;
    let wheel = fixture_wheel();
    let digest = Digest::of(&wheel);
    assert_eq!(
        upload_with_attestations(&harness.state, &wheel, &attestations_field(FILENAME, digest.as_str()),).await,
        StatusCode::OK
    );
    let provenance = harness
        .state
        .serving
        .meta
        .get_provenance("hosted", "peryxpkg", digest.as_str(), FILENAME)
        .unwrap()
        .unwrap()
        .0;
    assert!(
        harness
            .state
            .serving
            .blobs
            .delete(&Digest::from_hex(&provenance).unwrap())
            .await
            .unwrap()
    );

    let (status, ..) = get(&harness.state, &provenance_uri(digest.as_str()), None).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_upload_with_attestation_publishes_and_serves_provenance() {
    let harness = harness().await;
    let wheel = fixture_wheel();
    let sha256 = Digest::of(&wheel).as_str().to_owned();

    assert_eq!(
        upload_with_attestations(&harness.state, &wheel, &attestations_field(FILENAME, &sha256)).await,
        StatusCode::OK
    );

    let (_, _, detail) = get(&harness.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert!(detail.contains(&format!("/root/pypi/files/{sha256}/{FILENAME}.provenance")));

    let (status, headers, provenance) = get(&harness.state, &provenance_uri(&sha256), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "application/vnd.pypi.integrity.v1+json"
    );
    assert_eq!(headers["x-peryx-provenance-source"], "hosted");
    assert_eq!(headers["x-peryx-provenance-availability"], "cached");
    let document: serde_json::Value = serde_json::from_str(&provenance).unwrap();
    assert_eq!(document["version"], 1);
    assert_eq!(document["attestation_bundles"][0]["publisher"], serde_json::Value::Null);
    assert_eq!(
        document["attestation_bundles"][0]["attestations"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn test_project_page_summarizes_a_hosted_files_provenance() {
    let harness = harness().await;
    let wheel = fixture_wheel();
    let sha256 = Digest::of(&wheel).as_str().to_owned();
    assert_eq!(
        upload_with_attestations(&harness.state, &wheel, &attestations_field(FILENAME, &sha256)).await,
        StatusCode::OK
    );

    assert_eq!(
        browse_provenance(&harness, FILENAME).await,
        peryx_core::BrowseBadge {
            label: "hosted provenance".to_owned(),
            class: "provenance-valid".to_owned(),
            hint: Some(format!("{PUBLISH_PREDICATE}: matched")),
        }
    );
}

#[tokio::test]
async fn test_project_page_flags_an_unreadable_hosted_provenance() {
    let harness = harness().await;
    let wheel = fixture_wheel();
    let sha256 = Digest::of(&wheel).as_str().to_owned();
    assert_eq!(
        upload_with_attestations(&harness.state, &wheel, &attestations_field(FILENAME, &sha256)).await,
        StatusCode::OK
    );
    let provenance = harness
        .state
        .serving
        .meta
        .get_provenance("hosted", "peryxpkg", &sha256, FILENAME)
        .unwrap()
        .unwrap()
        .0;
    assert!(
        harness
            .state
            .serving
            .blobs
            .delete(&Digest::from_hex(&provenance).unwrap())
            .await
            .unwrap()
    );

    assert_eq!(
        browse_provenance(&harness, FILENAME).await,
        peryx_core::BrowseBadge {
            label: "hosted provenance".to_owned(),
            class: "provenance-malformed".to_owned(),
            hint: None,
        }
    );
}

async fn browse_provenance(harness: &Harness, filename: &str) -> peryx_core::BrowseBadge {
    let access = peryx_driver::access::ReadAccess::from_headers(&harness.state.serving, &axum::http::HeaderMap::new());
    let page = crate::serving::PypiServing
        .browse(BrowseRequest {
            state: harness.state.serving.clone(),
            position: 2,
            raw_query: format!("index=root%2Fpypi&project=peryxpkg&filename={filename}"),
            access: &access,
            base: None,
        })
        .await
        .unwrap()
        .unwrap();
    page.sections
        .into_iter()
        .find_map(|section| match section {
            peryx_core::BrowseSection::Table { heading, rows, .. } if heading == "Files" => rows
                .into_iter()
                .find(|row| row.cells.first().is_some_and(|cell| cell.text == filename))
                .and_then(|row| {
                    row.badges
                        .into_iter()
                        .find(|badge| badge.label.ends_with(" provenance"))
                }),
            _ => None,
        })
        .expect("the file carries a provenance badge")
}

#[tokio::test]
async fn test_upload_without_attestation_serves_no_provenance() {
    let harness = harness().await;
    let wheel = fixture_wheel();
    let sha256 = Digest::of(&wheel).as_str().to_owned();

    assert_eq!(
        upload_peryxpkg(&harness.state, "/root/pypi/", &wheel).await,
        StatusCode::OK
    );

    let (_, _, detail) = get(&harness.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert!(!detail.contains("provenance"));
    let (status, ..) = get(&harness.state, &provenance_uri(&sha256), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A cached index that publishes the file but advertised no attestation for it has no provenance
/// to serve, and says so without reaching upstream.
#[tokio::test]
async fn test_cached_file_without_an_attestation_serves_no_provenance() {
    let harness = harness().await;
    let digest = Digest::of(&fixture_wheel());
    crate::tests::register_publication(&harness.state.serving.meta, "pypi", FILENAME, digest.as_str(), None);

    let uri = format!("/pypi/files/{}/{FILENAME}.provenance", digest.as_str());
    let (status, ..) = get(&harness.state, &uri, None).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_subject_digest_mismatch_publishes_neither_object() {
    let harness = harness().await;
    let wheel = fixture_wheel();
    let sha256 = Digest::of(&wheel).as_str().to_owned();

    let status = upload_with_attestations(&harness.state, &wheel, &attestations_field(FILENAME, &"0".repeat(64))).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (page, ..) = get(&harness.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(page, StatusCode::NOT_FOUND);
    let (provenance, ..) = get(&harness.state, &provenance_uri(&sha256), None).await;
    assert_eq!(provenance, StatusCode::NOT_FOUND);
}

#[rstest]
#[case::malformed_json("{ not an array")]
#[case::empty_array("[]")]
#[case::not_an_object("[1]")]
#[tokio::test]
async fn test_malformed_attestations_are_rejected(#[case] field: &str) {
    let harness = harness().await;
    let wheel = fixture_wheel();

    let status = upload_with_attestations(&harness.state, &wheel, field).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (page, ..) = get(&harness.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(page, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_excessive_depth_is_rejected() {
    let harness = harness().await;
    let wheel = fixture_wheel();
    let deep = format!("[{}1{}]", "[".repeat(400), "]".repeat(400));

    assert_eq!(
        upload_with_attestations(&harness.state, &wheel, &deep).await,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn test_provenance_visibility_follows_yank_trash_and_restore() {
    let harness = harness().await;
    let wheel = fixture_wheel();
    let sha256 = Digest::of(&wheel).as_str().to_owned();
    upload_with_attestations(&harness.state, &wheel, &attestations_field(FILENAME, &sha256)).await;
    let provenance_marker = format!("{FILENAME}.provenance");

    request(
        &harness.state,
        "PUT",
        "/root/pypi/peryxpkg/1.0/yank",
        Some(&upload_auth()),
    )
    .await;
    let (_, _, yanked) = get(&harness.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert!(yanked.contains(&provenance_marker));

    request(&harness.state, "DELETE", "/root/pypi/peryxpkg/", Some(&upload_auth())).await;
    let (trashed, ..) = get(&harness.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(trashed, StatusCode::NOT_FOUND);

    request(
        &harness.state,
        "PUT",
        "/root/pypi/peryxpkg/restore",
        Some(&upload_auth()),
    )
    .await;
    let (_, _, restored) = get(&harness.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert!(restored.contains(&provenance_marker));
    let (fetch, ..) = get(&harness.state, &provenance_uri(&sha256), None).await;
    assert_eq!(fetch, StatusCode::OK);
}

#[tokio::test]
async fn test_untrusted_predicate_stays_out_of_the_html_page() {
    let harness = harness().await;
    let wheel = fixture_wheel();
    let sha256 = Digest::of(&wheel).as_str().to_owned();
    upload_with_attestations(&harness.state, &wheel, &attestations_field(FILENAME, &sha256)).await;

    let (_, _, html) = get(&harness.state, "/root/pypi/simple/peryxpkg/", Some("text/html")).await;

    assert!(html.contains(&format!(
        "data-provenance=\"/root/pypi/files/{sha256}/{FILENAME}.provenance\""
    )));
    assert!(!html.contains("<script>alert"));
    let (_, _, provenance) = get(&harness.state, &provenance_uri(&sha256), None).await;
    let document: serde_json::Value = serde_json::from_str(&provenance).unwrap();
    let statement = document["attestation_bundles"][0]["attestations"][0]["envelope"]["statement"]
        .as_str()
        .unwrap();
    let decoded = String::from_utf8(STANDARD.decode(statement).unwrap()).unwrap();
    assert!(decoded.contains(HOSTILE_PREDICATE));
}

fn attestations_field_of(predicate_type: &str, name: &str, sha256: &str) -> String {
    let statement = STANDARD.encode(
        serde_json::json!({
            "_type": "https://in-toto.io/Statement/v1",
            "subject": [{"name": name, "digest": {"sha256": sha256}}],
            "predicateType": predicate_type,
            "predicate": {},
        })
        .to_string(),
    );
    serde_json::json!([{
        "version": 1,
        "verification_material": {"certificate": "Zm9v", "transparency_entries": []},
        "envelope": {"statement": statement, "signature": "YmFy"},
    }])
    .to_string()
}

fn require_publish_predicate(mode: AttestationMode) -> Policy {
    policy(move |_neutral, pypi| {
        pypi.attestation_mode = mode;
        pypi.required_attestations = vec![PUBLISH_PREDICATE.to_owned()];
    })
}

async fn harness_requiring_publish(mode: AttestationMode) -> Harness {
    harness_with_policies(
        true,
        true,
        Policy::default(),
        require_publish_predicate(mode),
        Policy::default(),
    )
    .await
}

#[tokio::test]
async fn test_required_attestation_enforce_rejects_an_upload_without_attestations() {
    let harness = harness_requiring_publish(AttestationMode::Enforce).await;
    let wheel = fixture_wheel();

    let status = upload_peryxpkg(&harness.state, "/root/pypi/", &wheel).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    let (page, ..) = get(&harness.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(page, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_required_attestation_enforce_names_the_missing_predicate_type() {
    let harness = harness_requiring_publish(AttestationMode::Enforce).await;
    let wheel = fixture_wheel();
    let (content_type, body) = multipart_body(&upload_fields(), Some((FILENAME, &wheel)));

    let (status, body) =
        post_upload_response(&harness.state, "/root/pypi/", Some(&upload_auth()), &content_type, body).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    let denial: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(denial["rule"], "required-attestation");
    assert_eq!(
        denial["reason"],
        format!("upload is missing a required attestation predicate type: {PUBLISH_PREDICATE}")
    );
}

#[tokio::test]
async fn test_required_attestation_enforce_accepts_a_matching_upload() {
    let harness = harness_requiring_publish(AttestationMode::Enforce).await;
    let wheel = fixture_wheel();
    let sha256 = Digest::of(&wheel).as_str().to_owned();

    let status = upload_with_attestations(&harness.state, &wheel, &attestations_field(FILENAME, &sha256)).await;

    assert_eq!(status, StatusCode::OK);
    let (page, ..) = get(&harness.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(page, StatusCode::OK);
}

#[tokio::test]
async fn test_required_attestation_enforce_rejects_a_wrong_predicate_type() {
    let harness = harness_requiring_publish(AttestationMode::Enforce).await;
    let wheel = fixture_wheel();
    let sha256 = Digest::of(&wheel).as_str().to_owned();

    let status = upload_with_attestations(
        &harness.state,
        &wheel,
        &attestations_field_of(SLSA_PREDICATE, FILENAME, &sha256),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    let (page, ..) = get(&harness.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(page, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_required_attestation_audit_publishes_an_upload_without_attestations() {
    let harness = harness_requiring_publish(AttestationMode::Audit).await;
    let wheel = fixture_wheel();

    let status = upload_peryxpkg(&harness.state, "/root/pypi/", &wheel).await;

    assert_eq!(status, StatusCode::OK);
    let (page, ..) = get(&harness.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(page, StatusCode::OK);
}

const STAGING_SIGNATURE: &str = "c3RhZ2luZw==";
const PROD_SIGNATURE: &str = "cHJvZA==";

#[tokio::test]
async fn test_each_hosted_index_serves_the_bundle_its_own_publication_carries() {
    let h = promotion_harness().await;
    let wheel = fixture_wheel();
    let sha256 = Digest::of(&wheel).as_str().to_owned();
    for (route, signature) in [("staging", STAGING_SIGNATURE), ("prod", PROD_SIGNATURE)] {
        assert_eq!(
            upload_with_attestations_to(
                &h.state,
                &format!("/{route}/"),
                &wheel,
                &signed_attestations_field(FILENAME, &sha256, signature),
            )
            .await,
            StatusCode::OK,
            "{route} accepts its own publisher's attestation"
        );
    }

    for (route, signature) in [("staging", STAGING_SIGNATURE), ("prod", PROD_SIGNATURE)] {
        let (status, _, body) = get(
            &h.state,
            &format!("/{route}/files/{sha256}/{FILENAME}.provenance"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{route}");
        assert!(
            body.contains(signature),
            "{route} serves its own publication's bundle, not the other index's"
        );
    }
}

#[tokio::test]
async fn test_promotion_carries_the_bundle_onto_the_target_publication() {
    let h = authority_promotion_harness().await;
    let wheel = fixture_wheel();
    let sha256 = Digest::of(&wheel).as_str().to_owned();
    assert_eq!(
        upload_with_attestations_to(
            &h.state,
            "/staging/",
            &wheel,
            &signed_attestations_field(FILENAME, &sha256, STAGING_SIGNATURE),
        )
        .await,
        StatusCode::OK
    );

    assert_eq!(
        request(
            &h.state,
            "PUT",
            "/prod/peryxpkg/1.0/promote?from=staging",
            Some(&upload_auth()),
        )
        .await,
        StatusCode::OK
    );

    let (status, _, body) = get(&h.state, &format!("/prod/files/{sha256}/{FILENAME}.provenance"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(STAGING_SIGNATURE));
    let (_, _, page) = get(&h.state, "/prod/simple/peryxpkg/", Some("application/json")).await;
    let detail: serde_json::Value = serde_json::from_str(&page).unwrap();
    assert_eq!(
        detail["files"][0]["provenance"],
        serde_json::json!(format!("/prod/files/{sha256}/{FILENAME}.provenance")),
        "the promoted page points at the target's own bundle route"
    );
}

#[tokio::test]
async fn test_reupload_that_adds_attestations_is_rejected() {
    let h = harness().await;
    let wheel = fixture_wheel();
    let sha256 = Digest::of(&wheel).as_str().to_owned();
    assert_eq!(upload_with_attestations(&h.state, &wheel, "").await, StatusCode::OK);

    assert_eq!(
        upload_with_attestations(
            &h.state,
            &wheel,
            &signed_attestations_field(FILENAME, &sha256, STAGING_SIGNATURE),
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    let (status, ..) = get(&h.state, &provenance_uri(&sha256), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "the rejected bundle was never published");
}

#[rstest]
#[case::removed(None)]
#[case::changed(Some(PROD_SIGNATURE))]
#[tokio::test]
async fn test_reupload_cannot_drop_or_replace_the_published_attestations(#[case] second: Option<&str>) {
    let h = harness().await;
    let wheel = fixture_wheel();
    let sha256 = Digest::of(&wheel).as_str().to_owned();
    assert_eq!(
        upload_with_attestations(
            &h.state,
            &wheel,
            &signed_attestations_field(FILENAME, &sha256, STAGING_SIGNATURE),
        )
        .await,
        StatusCode::OK
    );

    let replacement = second.map_or_else(String::new, |signature| {
        signed_attestations_field(FILENAME, &sha256, signature)
    });
    assert_eq!(
        upload_with_attestations(&h.state, &wheel, &replacement).await,
        StatusCode::BAD_REQUEST
    );
    let (status, _, body) = get(&h.state, &provenance_uri(&sha256), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains(STAGING_SIGNATURE), "the published bundle still stands");
}

#[tokio::test]
async fn test_reupload_of_the_same_bytes_and_bundle_stays_idempotent() {
    let h = harness().await;
    let wheel = fixture_wheel();
    let sha256 = Digest::of(&wheel).as_str().to_owned();
    let field = attestations_field(FILENAME, &sha256);
    assert_eq!(upload_with_attestations(&h.state, &wheel, &field).await, StatusCode::OK);

    assert_eq!(upload_with_attestations(&h.state, &wheel, &field).await, StatusCode::OK);
}
