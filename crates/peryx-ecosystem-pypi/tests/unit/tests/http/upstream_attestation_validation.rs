//! Upstream PEP 740 validation, limits, logging, and failure tests.

use super::support::*;
use super::upstream_attestations::{
    FILENAME, PYPI_PROVENANCE, chunked_provenance_server, mount_provenance, mount_upstream_attestation_page_at,
    upstream_harness, upstream_page, upstream_provenance_uri,
};
use peryx_policy::RemoteMetadataMode;

#[rstest]
#[case::missing_content_type(None, PYPI_PROVENANCE, "upstream returned an invalid response")]
#[case::unsupported_content_type(Some("text/plain"), PYPI_PROVENANCE, "upstream returned an invalid response")]
#[case::malformed_json(Some("application/json"), "{", "could not be parsed")]
#[case::empty_bundles(Some("application/json"), r#"{"version":1,"attestation_bundles":[]}"#, "PEP 740")]
#[case::wrong_document_version(Some("application/json"), r#"{"version":2,"attestation_bundles":[{"publisher":{"kind":"test","claims":{}},"attestations":[{"version":1,"verification_material":{"certificate":"Zm9v","transparency_entries":[]},"envelope":{"statement":"e30=","signature":"YmFy"}}]}]}"#, "PEP 740")]
#[case::missing_publisher(
    Some("application/json"),
    r#"{"version":1,"attestation_bundles":[{"attestations":[{}]}]}"#,
    "PEP 740"
)]
#[case::empty_publisher_kind(Some("application/json"), r#"{"version":1,"attestation_bundles":[{"publisher":{"kind":"","claims":{}},"attestations":[{"version":1,"verification_material":{"certificate":"Zm9v","transparency_entries":[]},"envelope":{"statement":"e30=","signature":"YmFy"}}]}]}"#, "PEP 740")]
#[case::invalid_claims(
    Some("application/json"),
    r#"{"version":1,"attestation_bundles":[{"publisher":{"kind":"test","claims":[]},"attestations":[{}]}]}"#,
    "PEP 740"
)]
#[case::missing_claims(
    Some("application/json"),
    r#"{"version":1,"attestation_bundles":[{"publisher":{"kind":"test"},"attestations":[{}]}]}"#,
    "PEP 740"
)]
#[case::null_claims(
    Some("application/json"),
    r#"{"version":1,"attestation_bundles":[{"publisher":{"kind":"test","claims":null},"attestations":[{}]}]}"#,
    "PEP 740"
)]
#[case::empty_attestations(
    Some("application/json"),
    r#"{"version":1,"attestation_bundles":[{"publisher":{"kind":"test","claims":{}},"attestations":[]}]}"#,
    "PEP 740"
)]
#[case::incomplete_attestation(
    Some("application/json"),
    r#"{"version":1,"attestation_bundles":[{"publisher":{"kind":"test","claims":{}},"attestations":[{"version":1}]}]}"#,
    "PEP 740"
)]
#[case::wrong_attestation_version(Some("application/json"), r#"{"version":1,"attestation_bundles":[{"publisher":{"kind":"test","claims":{}},"attestations":[{"version":2,"verification_material":{"certificate":"Zm9v","transparency_entries":[]},"envelope":{"statement":"e30=","signature":"YmFy"}}]}]}"#, "PEP 740")]
#[tokio::test]
async fn test_upstream_attestation_rejects_an_invalid_response(
    #[case] content_type: Option<&str>,
    #[case] body: &str,
    #[case] message: &str,
) {
    let harness = upstream_harness(RemoteMetadataMode::Proxy).await;
    let digest = "a".repeat(64);
    let response = content_type.map_or_else(
        || ResponseTemplate::new(200).set_body_bytes(body.as_bytes()),
        |content_type| ResponseTemplate::new(200).set_body_raw(body, content_type),
    );
    mount_provenance(&harness, response).await;
    upstream_page(&harness, &digest, "application/json").await;

    let (status, _, body) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains(message), "{body}");
}

#[tokio::test(flavor = "current_thread")]
async fn test_upstream_attestation_retry_logs_redact_a_signed_url() {
    let harness = upstream_harness(RemoteMetadataMode::Proxy).await;
    let digest = "aa".repeat(32);
    let signed_url = format!(
        "{}/integrity/{FILENAME}.provenance?token=signed-secret#private-fragment",
        harness.server.uri()
    );
    mount_upstream_attestation_page_at(&harness, &digest, &signed_url).await;
    mount_provenance(&harness, ResponseTemplate::new(503)).await;
    let (page_status, ..) = get(&harness.state, "/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(page_status, StatusCode::OK);
    let logs = LogCapture::default();
    let guard = logs.install();

    let (status, ..) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;

    drop(guard);
    let text = logs.text();
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(text.contains(&format!("/integrity/{FILENAME}.provenance")), "{text}");
    assert!(!text.contains("signed-secret"), "{text}");
    assert!(!text.contains("private-fragment"), "{text}");
}

#[rstest]
#[case(RemoteMetadataMode::Proxy)]
#[case(RemoteMetadataMode::Cache)]
#[tokio::test]
async fn test_unexpected_upstream_attestation_304_has_no_body(#[case] mode: RemoteMetadataMode) {
    let harness = upstream_harness(mode).await;
    let digest = "c".repeat(64);
    mount_provenance(&harness, ResponseTemplate::new(304)).await;
    upstream_page(&harness, &digest, "application/json").await;

    let (status, ..) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn test_upstream_attestation_failure_does_not_hide_the_distribution() {
    let harness = upstream_harness(RemoteMetadataMode::Proxy).await;
    let digest = "8".repeat(64);
    mount_provenance(&harness, ResponseTemplate::new(404)).await;

    let page = upstream_page(&harness, &digest, "application/json").await;
    assert!(page.contains(FILENAME));
    let (status, ..) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (page_status, ..) = get(&harness.state, "/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(page_status, StatusCode::OK);
}

#[tokio::test]
async fn test_upstream_attestation_body_is_bounded() {
    let harness = upstream_harness(RemoteMetadataMode::Proxy).await;
    let digest = "9".repeat(64);
    mount_provenance(
        &harness,
        ResponseTemplate::new(200).set_body_raw(vec![b' '; 2 * 1024 * 1024 + 1], "application/json"),
    )
    .await;
    upstream_page(&harness, &digest, "application/json").await;

    let (status, _, body) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains("2097152-byte limit"));
}

#[tokio::test]
async fn test_upstream_attestation_chunked_body_is_bounded_without_a_content_length() {
    let harness = upstream_harness(RemoteMetadataMode::Proxy).await;
    let digest = "0c".repeat(32);
    let provenance = chunked_provenance_server(vec![b' '; 2 * 1024 * 1024 + 1]).await;
    mount_upstream_attestation_page_at(&harness, &digest, &provenance).await;
    let (page_status, ..) = get(&harness.state, "/pypi/simple/peryxpkg/", Some("application/json")).await;
    assert_eq!(page_status, StatusCode::OK);

    let (status, _, body) = get(&harness.state, &upstream_provenance_uri(&digest), None).await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains("2097152-byte limit"), "{body}");
}

#[tokio::test]
async fn test_proxy_mode_preserves_provenance_when_the_file_has_no_sha256() {
    let harness = upstream_harness(RemoteMetadataMode::Proxy).await;
    let provenance = format!("{}/integrity/{FILENAME}.provenance", harness.server.uri());
    let body = format!(
        r#"{{"meta":{{"api-version":"1.4"}},"name":"peryxpkg","files":[{{"filename":"{FILENAME}","url":"{server}/files/{FILENAME}","hashes":{{"md5":"abcd"}},"provenance":"{provenance}"}}]}}"#,
        server = harness.server.uri(),
    );
    Mock::given(method("GET"))
        .and(path("/simple/peryxpkg/"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "application/vnd.pypi.simple.v1+json"))
        .mount(&harness.server)
        .await;

    let detail = cache::resolve_detail(&harness.state.serving, harness.state.index_at(0), "peryxpkg", "pypi")
        .await
        .unwrap()
        .unwrap();

    assert_eq!(detail.files[0].provenance, Provenance::Url(provenance));
}
