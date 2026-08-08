use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{mount_get, simple_client};
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

#[tokio::test]
async fn test_fetch_bytes_reports_decode_errors() {
    let server = MockServer::start().await;
    mount_get(
        &server,
        "/files/pkg.bin",
        ResponseTemplate::new(200)
            .insert_header("content-encoding", "gzip")
            .set_body_bytes(b"not gzip".to_vec()),
    )
    .await;
    let client = simple_client(&server);
    let err = client
        .fetch_bytes(&format!("{}/files/pkg.bin", server.uri()))
        .await
        .unwrap_err();

    assert_eq!(err.user_message(), "upstream response could not be decoded");
}

#[tokio::test]
async fn test_fetch_bytes_reports_request_failures() {
    let client = UpstreamClient::new("https://upstream.example/artifacts/").unwrap();
    let err = client
        .fetch_bytes("http://peryx.nonexistent.invalid/pkg.bin")
        .await
        .unwrap_err();

    assert_eq!(err.user_message(), "upstream connection failed");
    assert_eq!(client.reachability().as_str(), "unreachable");
}

#[tokio::test]
async fn test_fetch_bytes_rejects_error_status() {
    let server = MockServer::start().await;
    mount_get(&server, "/files/pkg.bin", ResponseTemplate::new(500)).await;
    let client = simple_client(&server);
    let err = client
        .fetch_bytes(&format!("{}/files/pkg.bin", server.uri()))
        .await
        .unwrap_err();

    assert_eq!(err.user_message(), "upstream returned 500 Internal Server Error");
}

#[tokio::test]
async fn test_fetch_bytes_checks_status() {
    let server = MockServer::start().await;
    mount_get(&server, "/files/missing.bin", ResponseTemplate::new(404)).await;
    let client = simple_client(&server);

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
    let client = simple_client(&server);

    let err = client
        .stream_bytes(&format!("{}/files/missing.bin", server.uri()))
        .await
        .err()
        .unwrap();

    assert_eq!(err.status(), Some(404));
}

#[tokio::test]
async fn test_fetch_range_rejects_reversed_range() {
    let client = UpstreamClient::new("https://upstream.example/artifacts/").unwrap();

    let err = client
        .fetch_range("https://example.invalid/pkg.bin", 3, 1)
        .await
        .unwrap_err();

    assert_eq!(
        err.to_string(),
        "upstream returned an invalid byte range response: start 3 is after end 1"
    );
}

#[tokio::test]
async fn test_fetch_range_rejects_overflowing_range() {
    let client = UpstreamClient::new("https://upstream.example/artifacts/").unwrap();

    let err = client
        .fetch_range("https://example.invalid/pkg.bin", 0, u64::MAX)
        .await
        .unwrap_err();

    assert_eq!(
        err.to_string(),
        "upstream returned an invalid byte range response: requested range length overflowed"
    );
}

#[tokio::test]
async fn test_fetch_range_rejects_non_partial_success() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/pkg.bin"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let client = simple_client(&server);

    let err = client
        .fetch_range(&format!("{}/files/pkg.bin", server.uri()), 1, 3)
        .await
        .unwrap_err();

    assert_eq!(
        err.to_string(),
        "upstream returned an invalid byte range response: range request returned a non-206 success"
    );
}
