//! PEP 740 hosted attestations: upload binding, provenance discovery and fetch, visibility across
//! yank/trash/restore, and untrusted-content handling.

use super::support::*;
use crate::policy::AttestationMode;

const FILENAME: &str = "peryxpkg-1.0-py3-none-any.whl";
const PUBLISH_PREDICATE: &str = "https://docs.pypi.org/attestations/publish/v1";
const SLSA_PREDICATE: &str = "https://slsa.dev/provenance/v1";

/// A predicate string carrying HTML metacharacters, so a test can prove untrusted attestation
/// content never reaches the HTML page unescaped.
const HOSTILE_PREDICATE: &str = "<script>alert('xss')</script>";

fn statement(name: &str, sha256: &str) -> String {
    STANDARD.encode(
        serde_json::json!({
            "_type": "https://in-toto.io/Statement/v1",
            "subject": [{"name": name, "digest": {"sha256": sha256}}],
            "predicateType": "https://docs.pypi.org/attestations/publish/v1",
            "predicate": {"note": HOSTILE_PREDICATE},
        })
        .to_string(),
    )
}

fn attestations_field(name: &str, sha256: &str) -> String {
    serde_json::json!([{
        "version": 1,
        "verification_material": {"certificate": "Zm9v", "transparency_entries": []},
        "envelope": {"statement": statement(name, sha256), "signature": "YmFy"},
    }])
    .to_string()
}

async fn upload_with_attestations(state: &Arc<AppState>, wheel: &[u8], field: &str) -> StatusCode {
    let fields = vec![
        (":action", "file_upload"),
        ("name", "peryxpkg"),
        ("version", "1.0"),
        ("filetype", "bdist_wheel"),
        ("attestations", field),
    ];
    let (ct, body) = multipart_body(&fields, Some((FILENAME, wheel)));
    post_upload(state, "/root/pypi/", Some(&upload_auth()), &ct, body).await
}

fn provenance_uri(sha256: &str) -> String {
    format!("/root/pypi/files/{sha256}/{FILENAME}.provenance")
}

#[tokio::test]
async fn test_upload_with_attestation_publishes_and_serves_provenance() {
    let h = harness().await;
    let wheel = fixture_wheel();
    let sha = Digest::of(&wheel).as_str().to_owned();

    assert_eq!(
        upload_with_attestations(&h.state, &wheel, &attestations_field(FILENAME, &sha)).await,
        StatusCode::OK
    );

    let (_, _, detail) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert!(
        detail.contains(&format!("/root/pypi/files/{sha}/{FILENAME}.provenance")),
        "the simple JSON advertises the provenance URL: {detail}"
    );

    let (status, headers, provenance) = get(&h.state, &provenance_uri(&sha), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get(header::CONTENT_TYPE).unwrap(),
        "application/vnd.pypi.integrity.v1+json"
    );
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
async fn test_upload_without_attestation_serves_no_provenance() {
    let h = harness().await;
    let wheel = fixture_wheel();
    let sha = Digest::of(&wheel).as_str().to_owned();

    assert_eq!(upload_peryxpkg(&h.state, "/root/pypi/", &wheel).await, StatusCode::OK);

    let (_, _, detail) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert!(
        !detail.contains("provenance"),
        "no attestation, no provenance key: {detail}"
    );
    let (status, ..) = get(&h.state, &provenance_uri(&sha), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_subject_digest_mismatch_publishes_neither_object() {
    let h = harness().await;
    let wheel = fixture_wheel();
    let sha = Digest::of(&wheel).as_str().to_owned();
    let wrong = "0".repeat(64);

    let status = upload_with_attestations(&h.state, &wheel, &attestations_field(FILENAME, &wrong)).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (page, ..) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(page, StatusCode::NOT_FOUND, "the distribution is not published either");
    let (provenance, ..) = get(&h.state, &provenance_uri(&sha), None).await;
    assert_eq!(provenance, StatusCode::NOT_FOUND);
}

#[rstest]
#[case::malformed_json("{ not an array")]
#[case::empty_array("[]")]
#[case::not_an_object("[1]")]
#[tokio::test]
async fn test_malformed_attestations_are_rejected(#[case] field: &str) {
    let h = harness().await;
    let wheel = fixture_wheel();

    let status = upload_with_attestations(&h.state, &wheel, field).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (page, ..) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(page, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_excessive_depth_is_rejected() {
    let h = harness().await;
    let wheel = fixture_wheel();
    let deep = format!("[{}1{}]", "[".repeat(400), "]".repeat(400));

    assert_eq!(
        upload_with_attestations(&h.state, &wheel, &deep).await,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn test_provenance_visibility_follows_yank_trash_and_restore() {
    let h = harness().await;
    let wheel = fixture_wheel();
    let sha = Digest::of(&wheel).as_str().to_owned();
    upload_with_attestations(&h.state, &wheel, &attestations_field(FILENAME, &sha)).await;

    let provenance_marker = format!("{FILENAME}.provenance");

    // Yanking keeps the file visible, so its provenance stays advertised.
    request(&h.state, "PUT", "/root/pypi/peryxpkg/1.0/yank", Some(&upload_auth())).await;
    let (_, _, yanked) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert!(
        yanked.contains(&provenance_marker),
        "a yanked file keeps its provenance"
    );

    // Trashing hides the file, so the provenance association disappears from the page.
    request(&h.state, "DELETE", "/root/pypi/peryxpkg/", Some(&upload_auth())).await;
    let (trashed_status, ..) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(trashed_status, StatusCode::NOT_FOUND);

    // Restoring brings the file back with its provenance intact.
    request(&h.state, "PUT", "/root/pypi/peryxpkg/restore", Some(&upload_auth())).await;
    let (_, _, restored) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert!(
        restored.contains(&provenance_marker),
        "a restored file regains its provenance"
    );
    let (fetch, ..) = get(&h.state, &provenance_uri(&sha), None).await;
    assert_eq!(
        fetch,
        StatusCode::OK,
        "the provenance blob survived the trash and restore"
    );
}

#[tokio::test]
async fn test_untrusted_predicate_stays_out_of_the_html_page() {
    let h = harness().await;
    let wheel = fixture_wheel();
    let sha = Digest::of(&wheel).as_str().to_owned();
    upload_with_attestations(&h.state, &wheel, &attestations_field(FILENAME, &sha)).await;

    let (_, _, html) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("text/html")).await;

    assert!(
        html.contains(&format!(
            "data-provenance=\"/root/pypi/files/{sha}/{FILENAME}.provenance\""
        )),
        "the HTML links the provenance by URL: {html}"
    );
    assert!(
        !html.contains("<script>alert"),
        "the untrusted predicate never reaches the HTML page: {html}"
    );

    // The predicate is served only inside the JSON provenance body, where its metacharacters stay
    // inert string data rather than active markup.
    let (_, _, provenance) = get(&h.state, &provenance_uri(&sha), None).await;
    let document: serde_json::Value = serde_json::from_str(&provenance).unwrap();
    let statement = document["attestation_bundles"][0]["attestations"][0]["envelope"]["statement"]
        .as_str()
        .unwrap();
    let decoded = String::from_utf8(STANDARD.decode(statement).unwrap()).unwrap();
    assert!(
        decoded.contains(HOSTILE_PREDICATE),
        "the predicate round-trips verbatim in the bundle"
    );
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
    let h = harness_requiring_publish(AttestationMode::Enforce).await;
    let wheel = fixture_wheel();

    let status = upload_peryxpkg(&h.state, "/root/pypi/", &wheel).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    let (page, ..) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(page, StatusCode::NOT_FOUND, "the distribution never publishes");
}

#[tokio::test]
async fn test_required_attestation_enforce_names_the_missing_predicate_type() {
    let h = harness_requiring_publish(AttestationMode::Enforce).await;
    let wheel = fixture_wheel();
    let (content_type, body) = multipart_body(&upload_fields(), Some((FILENAME, &wheel)));

    let (status, body) = post_upload_response(&h.state, "/root/pypi/", Some(&upload_auth()), &content_type, body).await;

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
    let h = harness_requiring_publish(AttestationMode::Enforce).await;
    let wheel = fixture_wheel();
    let sha = Digest::of(&wheel).as_str().to_owned();

    let status = upload_with_attestations(&h.state, &wheel, &attestations_field(FILENAME, &sha)).await;

    assert_eq!(status, StatusCode::OK);
    let (page, ..) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(page, StatusCode::OK);
}

#[tokio::test]
async fn test_required_attestation_enforce_rejects_a_wrong_predicate_type() {
    let h = harness_requiring_publish(AttestationMode::Enforce).await;
    let wheel = fixture_wheel();
    let sha = Digest::of(&wheel).as_str().to_owned();

    let status =
        upload_with_attestations(&h.state, &wheel, &attestations_field_of(SLSA_PREDICATE, FILENAME, &sha)).await;

    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "a bound attestation of the wrong type does not satisfy the rule"
    );
    let (page, ..) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(page, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_required_attestation_audit_publishes_an_upload_without_attestations() {
    let h = harness_requiring_publish(AttestationMode::Audit).await;
    let wheel = fixture_wheel();

    let status = upload_peryxpkg(&h.state, "/root/pypi/", &wheel).await;

    assert_eq!(status, StatusCode::OK, "audit mode observes but does not block");
    let (page, ..) = get(&h.state, "/root/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(page, StatusCode::OK);
}
