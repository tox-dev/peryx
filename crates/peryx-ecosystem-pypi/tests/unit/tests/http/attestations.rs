use super::support::*;
use crate::policy::AttestationMode;
use peryx_driver::serving::{BrowseDriver as _, BrowseRequest};

const FILENAME: &str = "peryxpkg-1.0-py3-none-any.whl";
pub(super) const SIGNED_FILENAME: &str = "circleci_sign_publish_example-0.0.1.dev137-py3-none-any.whl";
pub(super) const SIGNED_PROJECT: &str = "circleci-sign-publish-example";
pub(super) const SIGNED_VERSION: &str = "0.0.1.dev137";
const PUBLISH_PREDICATE: &str = "https://docs.pypi.org/attestations/publish/v1";
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

fn attestations_field(name: &str, sha256: &str) -> String {
    dummy_attestations_field_with_signature(name, sha256, "YmFy")
}

fn dummy_attestations_field_with_signature(name: &str, sha256: &str, signature: &str) -> String {
    serde_json::json!([{
        "version": 1,
        "verification_material": {"certificate": "Zm9v", "transparency_entries": []},
        "envelope": {"statement": statement(name, sha256), "signature": signature},
    }])
    .to_string()
}

async fn upload_with_attestations(state: &Arc<AppState>, wheel: &[u8], field: &str) -> StatusCode {
    upload_with_attestations_to(state, "/root/pypi/", wheel, field).await
}

pub(super) fn signed_distribution() -> Vec<u8> {
    STANDARD
        .decode(
            include_str!("../../../fixtures/circleci_sign_publish_example-0.0.1.dev137-py3-none-any.whl.b64")
                .split_whitespace()
                .collect::<String>(),
        )
        .unwrap()
}

pub(super) fn signed_attestations_field() -> String {
    format!(
        "[{}]",
        include_str!(
            "../../../fixtures/circleci_sign_publish_example-0.0.1.dev137-py3-none-any.whl.publish.attestation"
        )
    )
}

fn signed_slsa_attestations_field() -> String {
    format!(
        "[{}]",
        include_str!("../../../fixtures/pypi_attestations-0.0.19.tar.gz.slsa.attestation")
    )
}

pub(super) async fn upload_signed_attestation(state: &Arc<AppState>, route: &str, field: &str) -> StatusCode {
    upload_signed_attestation_response(state, route, field).await.0
}

pub(super) fn mark_provenance_legacy(
    state: &Arc<AppState>,
    index: &str,
    project: &str,
    artifact_sha256: &str,
    filename: &str,
) {
    let (provenance_sha256, size) = state
        .serving
        .meta
        .get_provenance(index, project, artifact_sha256, filename)
        .unwrap()
        .unwrap();
    state
        .serving
        .meta
        .put_driver_value(
            &format!("pypi\0a\0{index}/{project}/{artifact_sha256}/{filename}"),
            format!("{provenance_sha256}\n{size}").as_bytes(),
        )
        .unwrap();
}

async fn upload_signed_attestation_response(state: &Arc<AppState>, route: &str, field: &str) -> (StatusCode, String) {
    let route_resource = route.trim_matches('/');
    let resource = format!("{route_resource}/{SIGNED_PROJECT}");
    let publisher = match route_resource {
        "staging" => "staging",
        "prod" => "prod",
        _ => "release",
    };
    let token = super::upload::trusted_publisher_token(
        &peryx_identity::Signer::new(b"realm-key", "peryx").unwrap(),
        &resource,
        publisher,
    );
    upload_signed_attestation_with_auth(state, route, field, &format!("Bearer {token}")).await
}

async fn upload_signed_attestation_with_auth(
    state: &Arc<AppState>,
    route: &str,
    field: &str,
    auth: &str,
) -> (StatusCode, String) {
    let artifact = signed_distribution();
    let fields = vec![
        (":action", "file_upload"),
        ("name", SIGNED_PROJECT),
        ("version", SIGNED_VERSION),
        ("filetype", "bdist_wheel"),
        ("attestations", field),
    ];
    let (content_type, body) = multipart_body(&fields, Some((SIGNED_FILENAME, &artifact)));
    post_upload_response(state, route, Some(auth), &content_type, body).await
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

fn signed_provenance_uri(sha256: &str) -> String {
    format!("/root/pypi/files/{sha256}/{SIGNED_FILENAME}.provenance")
}

#[tokio::test]
async fn test_basic_identity_named_as_a_publisher_has_no_attestation_context() {
    let harness = harness().await;
    let auth = format!("Basic {}", STANDARD.encode("__token__:basic-publisher"));

    assert_eq!(
        upload_signed_attestation_with_auth(&harness.state, "/root/pypi/", &signed_attestations_field(), &auth)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn test_hosted_provenance_with_a_missing_blob_is_not_found() {
    let harness = harness().await;
    let digest = Digest::of(&signed_distribution());
    assert_eq!(
        upload_signed_attestation(&harness.state, "/root/pypi/", &signed_attestations_field()).await,
        StatusCode::OK
    );
    let provenance = harness
        .state
        .serving
        .meta
        .get_provenance("hosted", SIGNED_PROJECT, digest.as_str(), SIGNED_FILENAME)
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

    let (status, ..) = get(&harness.state, &signed_provenance_uri(digest.as_str()), None).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A bundle carries certificates and transparency proofs, so its field may run far past the 64 KiB
/// that bounds every other upload text field.
#[tokio::test]
async fn test_upload_accepts_an_attestation_field_past_the_text_field_limit() {
    let harness = harness().await;
    let field = format!("{}{}", signed_attestations_field(), " ".repeat(64 * 1024));

    assert_eq!(
        upload_signed_attestation(&harness.state, "/root/pypi/", &field).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn test_upload_with_attestation_publishes_and_serves_provenance() {
    let harness = harness().await;
    let sha256 = Digest::of(&signed_distribution()).as_str().to_owned();

    assert_eq!(
        upload_signed_attestation(&harness.state, "/root/pypi/", &signed_attestations_field()).await,
        StatusCode::OK
    );

    let (status, headers, provenance) = get(&harness.state, &signed_provenance_uri(&sha256), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "application/vnd.pypi.integrity.v1+json"
    );
    assert_eq!(headers["x-peryx-provenance-source"], "hosted");
    assert_eq!(headers["x-peryx-provenance-availability"], "cached");
    let document: serde_json::Value = serde_json::from_str(&provenance).unwrap();
    assert_eq!(document["version"], 1);
    assert_eq!(
        document["attestation_bundles"][0]["publisher"],
        serde_json::json!({
            "kind": "https://oidc.circleci.com",
            "claims": {
                "identity": "https://circleci.com/api/v2/projects/fdd9283f-e619-46af-8f9c-851f7d3e8b2b/pipeline-definitions/8e4f8ab2-8d7c-4827-9f15-de076d6d647f",
                "1.3.6.1.4.1.57264.1.12": "github.com/CircleCI-Public/sign-and-publish-examples",
            },
        })
    );
    assert_eq!(
        document["attestation_bundles"][0]["attestations"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn test_legacy_hosted_provenance_is_not_advertised_or_served() {
    let harness = harness().await;
    let sha256 = Digest::of(&signed_distribution()).as_str().to_owned();
    assert_eq!(
        upload_signed_attestation(&harness.state, "/root/pypi/", &signed_attestations_field()).await,
        StatusCode::OK
    );
    mark_provenance_legacy(&harness.state, "hosted", SIGNED_PROJECT, &sha256, SIGNED_FILENAME);

    let (_, _, json) = get(
        &harness.state,
        &format!("/root/pypi/simple/{SIGNED_PROJECT}/"),
        Some("application/json"),
    )
    .await;
    let detail: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(detail["files"][0].get("provenance").is_none());
    let (_, _, html) = get(
        &harness.state,
        &format!("/root/pypi/simple/{SIGNED_PROJECT}/"),
        Some("text/html"),
    )
    .await;
    assert!(!html.contains("data-provenance"));
    assert_eq!(
        get(&harness.state, &signed_provenance_uri(&sha256), None).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn test_project_page_summarizes_a_hosted_files_provenance() {
    let harness = harness().await;
    assert_eq!(
        upload_signed_attestation(&harness.state, "/root/pypi/", &signed_attestations_field()).await,
        StatusCode::OK
    );

    assert_eq!(
        browse_provenance(&harness, SIGNED_PROJECT, SIGNED_FILENAME).await,
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
    let sha256 = Digest::of(&signed_distribution()).as_str().to_owned();
    assert_eq!(
        upload_signed_attestation(&harness.state, "/root/pypi/", &signed_attestations_field()).await,
        StatusCode::OK
    );
    let provenance = harness
        .state
        .serving
        .meta
        .get_provenance("hosted", SIGNED_PROJECT, &sha256, SIGNED_FILENAME)
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
        browse_provenance(&harness, SIGNED_PROJECT, SIGNED_FILENAME).await,
        peryx_core::BrowseBadge {
            label: "hosted provenance".to_owned(),
            class: "provenance-malformed".to_owned(),
            hint: None,
        }
    );
}

async fn browse_provenance(harness: &Harness, project: &str, filename: &str) -> peryx_core::BrowseBadge {
    let access = peryx_driver::access::ReadAccess::from_headers(&harness.state.serving, &axum::http::HeaderMap::new());
    let page = crate::serving::PypiServing
        .browse(BrowseRequest {
            state: harness.state.serving.clone(),
            position: 2,
            raw_query: format!("index=root%2Fpypi&project={project}&filename={filename}"),
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
    let sha256 = Digest::of(&signed_distribution()).as_str().to_owned();
    upload_signed_attestation(&harness.state, "/root/pypi/", &signed_attestations_field()).await;
    let provenance_marker = format!("{SIGNED_FILENAME}.provenance");

    request(
        &harness.state,
        "PUT",
        "/root/pypi/circleci-sign-publish-example/0.0.1.dev137/yank",
        Some(&upload_auth()),
    )
    .await;
    let (_, _, yanked) = get(
        &harness.state,
        "/root/pypi/simple/circleci-sign-publish-example/",
        Some("application/json"),
    )
    .await;
    assert!(yanked.contains(&provenance_marker));

    request(
        &harness.state,
        "DELETE",
        "/root/pypi/circleci-sign-publish-example/",
        Some(&upload_auth()),
    )
    .await;
    let (trashed, ..) = get(
        &harness.state,
        "/root/pypi/simple/circleci-sign-publish-example/",
        Some("application/json"),
    )
    .await;
    assert_eq!(trashed, StatusCode::NOT_FOUND);

    request(
        &harness.state,
        "PUT",
        "/root/pypi/circleci-sign-publish-example/restore",
        Some(&upload_auth()),
    )
    .await;
    let (_, _, restored) = get(
        &harness.state,
        "/root/pypi/simple/circleci-sign-publish-example/",
        Some("application/json"),
    )
    .await;
    assert!(restored.contains(&provenance_marker));
    let (fetch, ..) = get(&harness.state, &signed_provenance_uri(&sha256), None).await;
    assert_eq!(fetch, StatusCode::OK);
}

#[tokio::test]
async fn test_tampered_predicate_is_not_published() {
    let harness = harness().await;
    let mut attestation: serde_json::Value = serde_json::from_str(&signed_attestations_field()).unwrap();
    let statement = STANDARD
        .decode(attestation[0]["envelope"]["statement"].as_str().unwrap())
        .unwrap();
    let mut statement: serde_json::Value = serde_json::from_slice(&statement).unwrap();
    statement["predicate"] = serde_json::json!({"note": HOSTILE_PREDICATE});
    attestation[0]["envelope"]["statement"] =
        serde_json::Value::String(STANDARD.encode(serde_json::to_vec(&statement).unwrap()));

    assert_eq!(
        upload_signed_attestation(&harness.state, "/root/pypi/", &attestation.to_string()).await,
        StatusCode::BAD_REQUEST
    );
    let (status, _, _) = get(
        &harness.state,
        "/root/pypi/simple/circleci-sign-publish-example/",
        Some("text/html"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
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

    let status = upload_signed_attestation(&harness.state, "/root/pypi/", &signed_attestations_field()).await;

    assert_eq!(status, StatusCode::OK);
    let digest = Digest::of(&signed_distribution());
    assert_eq!(
        get(&harness.state, &signed_provenance_uri(digest.as_str()), None)
            .await
            .0,
        StatusCode::OK
    );
}

#[tokio::test]
async fn test_required_attestation_rejects_an_unrelated_signed_artifact() {
    let harness = harness_requiring_publish(AttestationMode::Enforce).await;

    let status = upload_signed_attestation(&harness.state, "/root/pypi/", &signed_slsa_attestations_field()).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (page, ..) = get(
        &harness.state,
        "/root/pypi/simple/pypi-attestations/",
        Some("application/json"),
    )
    .await;
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

#[tokio::test]
async fn test_each_hosted_index_serves_the_bundle_its_own_publication_carries() {
    let h = promotion_harness().await;
    let sha256 = Digest::of(&signed_distribution()).as_str().to_owned();
    let one = signed_attestations_field();
    let attestation: serde_json::Value = serde_json::from_str(&one).unwrap();
    let two = serde_json::json!([attestation[0], attestation[0]]).to_string();
    for (route, field) in [("staging", one), ("prod", two)] {
        assert_eq!(
            upload_signed_attestation(&h.state, &format!("/{route}/"), &field).await,
            StatusCode::OK,
            "{route} accepts its own publisher's attestation"
        );
    }

    for (route, count) in [("staging", 1), ("prod", 2)] {
        let (status, _, body) = get(
            &h.state,
            &format!("/{route}/files/{sha256}/{SIGNED_FILENAME}.provenance"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{route}");
        let document: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            document["attestation_bundles"][0]["attestations"]
                .as_array()
                .unwrap()
                .len(),
            count
        );
    }
}

#[tokio::test]
async fn test_promotion_carries_the_bundle_onto_the_target_publication() {
    let h = authority_promotion_harness().await;
    let sha256 = Digest::of(&signed_distribution()).as_str().to_owned();
    assert_eq!(
        upload_signed_attestation(&h.state, "/staging/", &signed_attestations_field()).await,
        StatusCode::OK
    );
    let (_, _, source) = get(
        &h.state,
        &format!("/staging/files/{sha256}/{SIGNED_FILENAME}.provenance"),
        None,
    )
    .await;

    assert_eq!(
        request(
            &h.state,
            "PUT",
            &format!("/prod/{SIGNED_PROJECT}/{SIGNED_VERSION}/promote?from=staging"),
            Some(&upload_auth()),
        )
        .await,
        StatusCode::OK
    );

    let (status, _, body) = get(
        &h.state,
        &format!("/prod/files/{sha256}/{SIGNED_FILENAME}.provenance"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, source);
    let (_, _, page) = get(
        &h.state,
        &format!("/prod/simple/{SIGNED_PROJECT}/"),
        Some("application/json"),
    )
    .await;
    let detail: serde_json::Value = serde_json::from_str(&page).unwrap();
    assert_eq!(
        detail["files"][0]["provenance"],
        serde_json::json!(format!("/prod/files/{sha256}/{SIGNED_FILENAME}.provenance")),
        "the promoted page points at the target's own bundle route"
    );
}

#[tokio::test]
async fn test_promotion_omits_legacy_provenance_from_the_target() {
    let h = authority_promotion_harness().await;
    let sha256 = Digest::of(&signed_distribution()).as_str().to_owned();
    assert_eq!(
        upload_signed_attestation(&h.state, "/staging/", &signed_attestations_field()).await,
        StatusCode::OK
    );
    mark_provenance_legacy(&h.state, "staging", SIGNED_PROJECT, &sha256, SIGNED_FILENAME);

    assert_eq!(
        request(
            &h.state,
            "PUT",
            &format!("/prod/{SIGNED_PROJECT}/{SIGNED_VERSION}/promote?from=staging"),
            Some(&upload_auth()),
        )
        .await,
        StatusCode::OK
    );

    let (_, _, page) = get(
        &h.state,
        &format!("/prod/simple/{SIGNED_PROJECT}/"),
        Some("application/json"),
    )
    .await;
    let detail: serde_json::Value = serde_json::from_str(&page).unwrap();
    assert!(detail["files"][0].get("provenance").is_none());
    assert_eq!(
        get(
            &h.state,
            &format!("/prod/files/{sha256}/{SIGNED_FILENAME}.provenance"),
            None,
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn test_reupload_that_adds_attestations_is_rejected() {
    let h = harness().await;
    let sha256 = Digest::of(&signed_distribution()).as_str().to_owned();
    assert_eq!(
        upload_signed_attestation(&h.state, "/root/pypi/", "").await,
        StatusCode::OK
    );

    assert_eq!(
        upload_signed_attestation(&h.state, "/root/pypi/", &signed_attestations_field()).await,
        StatusCode::BAD_REQUEST
    );
    let (status, ..) = get(&h.state, &signed_provenance_uri(&sha256), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "the rejected bundle was never published");
}

#[rstest]
#[case::removed(false)]
#[case::changed(true)]
#[tokio::test]
async fn test_reupload_cannot_drop_or_replace_the_published_attestations(#[case] replace: bool) {
    let h = harness().await;
    let sha256 = Digest::of(&signed_distribution()).as_str().to_owned();
    assert_eq!(
        upload_signed_attestation(&h.state, "/root/pypi/", &signed_attestations_field()).await,
        StatusCode::OK
    );

    let replacement = replace.then(signed_slsa_attestations_field).unwrap_or_default();
    assert_eq!(
        upload_signed_attestation(&h.state, "/root/pypi/", &replacement).await,
        StatusCode::BAD_REQUEST
    );
    let (status, _, body) = get(&h.state, &signed_provenance_uri(&sha256), None).await;
    assert_eq!(status, StatusCode::OK);
    let original: serde_json::Value = serde_json::from_str(&signed_attestations_field()).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&body).unwrap()["attestation_bundles"][0]["attestations"][0],
        original[0]
    );
}

#[tokio::test]
async fn test_reupload_of_the_same_bytes_and_bundle_stays_idempotent() {
    let h = harness().await;
    let field = signed_attestations_field();
    assert_eq!(
        upload_signed_attestation(&h.state, "/root/pypi/", &field).await,
        StatusCode::OK
    );

    assert_eq!(
        upload_signed_attestation(&h.state, "/root/pypi/", &field).await,
        StatusCode::OK
    );
}
