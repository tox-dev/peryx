use std::time::Duration;

use rstest::rstest;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{guarded_client, mount_get};
use crate::client::{CredentialError, UpstreamClient, UpstreamError};

#[test]
fn test_credential_error_is_redacted_for_users() {
    let error = UpstreamError::Credential(CredentialError::new("secret file /run/keys/token is empty"));

    assert_eq!(error.status(), None);
    assert_eq!(error.user_message(), "upstream credential refresh failed");
    assert_eq!(
        error.to_string(),
        "upstream credential refresh failed: secret file /run/keys/token is empty"
    );
}

#[test]
fn test_blocked_destination_hides_reason_from_users() {
    let error = UpstreamError::BlockedDestination {
        reason: "169.254.169.254 is not a public address".to_owned(),
    };

    assert_eq!(error.status(), None);
    assert_eq!(error.user_message(), "upstream destination is not permitted");
    assert_eq!(
        error.to_string(),
        "upstream destination is not permitted: 169.254.169.254 is not a public address"
    );
}

#[test]
fn test_invalid_response_hides_reason_from_users() {
    let error = UpstreamError::InvalidResponse {
        reason: "signed URL contained credentials".to_owned(),
    };

    assert_eq!(error.status(), None);
    assert_eq!(error.user_message(), "upstream returned an invalid response");
    assert_eq!(
        error.to_string(),
        "invalid upstream response: signed URL contained credentials"
    );
}

#[tokio::test]
async fn test_fetch_bytes_reports_decode_errors() {
    let server = MockServer::start().await;
    mount_get(
        &server,
        "/files/artifact.bin",
        ResponseTemplate::new(200)
            .insert_header("content-encoding", "gzip")
            .set_body_bytes(b"not gzip".to_vec()),
    )
    .await;
    let client = guarded_client(&server);
    let err = client
        .fetch_bytes(&format!("{}/files/artifact.bin", server.uri()))
        .await
        .unwrap_err();

    assert_eq!(err.user_message(), "upstream response could not be decoded");
}

#[tokio::test]
async fn test_fetch_bytes_reports_request_failures() {
    let client = UpstreamClient::new("https://upstream.example/artifacts/").unwrap();
    let err = client
        .fetch_bytes("http://peryx.nonexistent.invalid/artifact.bin")
        .await
        .unwrap_err();

    assert_eq!(err.user_message(), "upstream connection failed");
    assert_eq!(client.reachability().as_str(), "unreachable");
}

#[tokio::test]
async fn test_timeout_error_has_specific_user_message() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(1)))
        .mount(&server)
        .await;
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let error = reqwest::Client::new()
        .get(server.uri())
        .timeout(Duration::from_millis(1))
        .send()
        .await
        .unwrap_err();

    assert_eq!(UpstreamError::from(error).user_message(), "upstream request timed out");
}

#[tokio::test]
async fn test_fetch_bytes_rejects_error_status() {
    let server = MockServer::start().await;
    mount_get(&server, "/files/artifact.bin", ResponseTemplate::new(500)).await;
    let client = guarded_client(&server);
    let err = client
        .fetch_bytes(&format!("{}/files/artifact.bin", server.uri()))
        .await
        .unwrap_err();

    assert_eq!(err.user_message(), "upstream returned 500 Internal Server Error");
}

#[tokio::test]
async fn test_fetch_bytes_checks_status() {
    let server = MockServer::start().await;
    mount_get(&server, "/files/missing.bin", ResponseTemplate::new(404)).await;
    let client = guarded_client(&server);

    let err = client
        .fetch_bytes(&format!("{}/files/missing.bin", server.uri()))
        .await
        .unwrap_err();

    assert_eq!(err.status(), Some(404));
}

#[tokio::test]
async fn test_stream_bytes_checks_status() {
    let server = MockServer::start().await;
    mount_get(&server, "/files/missing.bin", ResponseTemplate::new(404)).await;
    let client = guarded_client(&server);

    let err = client
        .stream_bytes(&format!("{}/files/missing.bin", server.uri()))
        .await
        .err()
        .unwrap();

    assert_eq!(err.status(), Some(404));
}

#[rstest]
#[case::reversed(3, 1, "start 3 is after end 1")]
#[case::overflow(0, u64::MAX, "requested range length overflowed")]
#[tokio::test]
async fn test_fetch_range_rejects_invalid_bounds(#[case] start: u64, #[case] end: u64, #[case] expected: &str) {
    let client = UpstreamClient::new("https://upstream.example/artifacts/").unwrap();

    let err = client
        .fetch_range("https://example.invalid/artifact.bin", start, end, usize::MAX)
        .await
        .unwrap_err();

    assert_eq!(
        err.to_string(),
        format!("upstream returned an invalid byte range response: {expected}")
    );
}

#[tokio::test]
async fn test_fetch_range_rejects_non_partial_success() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/artifact.bin"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let client = guarded_client(&server);

    let err = client
        .fetch_range(&format!("{}/files/artifact.bin", server.uri()), 1, 3, 3)
        .await
        .unwrap_err();

    assert_eq!(
        err.to_string(),
        "upstream returned an invalid byte range response: range request returned a non-206 success"
    );
}
