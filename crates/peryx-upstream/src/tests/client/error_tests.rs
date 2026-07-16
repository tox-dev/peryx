use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{mount_get, simple_client};
use crate::client::UpstreamClient;

#[tokio::test]
async fn test_fetch_bytes_reports_decode_errors() {
    let server = MockServer::start().await;
    mount_get(
        &server,
        "/files/pkg.whl",
        ResponseTemplate::new(200)
            .insert_header("content-encoding", "gzip")
            .set_body_bytes(b"not gzip".to_vec()),
    )
    .await;
    let client = simple_client(&server);
    let err = client
        .fetch_bytes(&format!("{}/files/pkg.whl", server.uri()))
        .await
        .unwrap_err();

    assert_eq!(err.user_message(), "upstream response could not be decoded");
}

#[tokio::test]
async fn test_fetch_bytes_reports_request_failures() {
    let client = UpstreamClient::new("https://pypi.org/simple/").unwrap();
    let err = client.fetch_bytes("ftp://example.invalid/pkg.whl").await.unwrap_err();

    assert_eq!(err.user_message(), "upstream request failed");
    assert_eq!(client.reachability().as_str(), "unreachable");
}

#[tokio::test]
async fn test_fetch_bytes_rejects_error_status() {
    let server = MockServer::start().await;
    mount_get(&server, "/files/pkg.whl", ResponseTemplate::new(500)).await;
    let client = simple_client(&server);
    let err = client
        .fetch_bytes(&format!("{}/files/pkg.whl", server.uri()))
        .await
        .unwrap_err();

    assert_eq!(err.user_message(), "upstream returned 500 Internal Server Error");
}

#[tokio::test]
async fn test_fetch_bytes_checks_status() {
    let server = MockServer::start().await;
    mount_get(&server, "/files/missing.whl", ResponseTemplate::new(404)).await;
    let client = simple_client(&server);

    let err = client
        .fetch_bytes(&format!("{}/files/missing.whl", server.uri()))
        .await
        .unwrap_err();

    assert_eq!(err.status(), Some(404));
}

#[tokio::test]
async fn test_stream_bytes_checks_status() {
    let server = MockServer::start().await;
    mount_get(&server, "/files/missing.whl", ResponseTemplate::new(404)).await;
    let client = simple_client(&server);

    let err = client
        .stream_bytes(&format!("{}/files/missing.whl", server.uri()))
        .await
        .err()
        .unwrap();

    assert_eq!(err.status(), Some(404));
}

#[tokio::test]
async fn test_fetch_range_rejects_reversed_range() {
    let client = UpstreamClient::new("https://pypi.org/simple/").unwrap();

    let err = client
        .fetch_range("https://example.invalid/pkg.whl", 3, 1)
        .await
        .unwrap_err();

    assert_eq!(
        err.to_string(),
        "upstream returned an invalid byte range response: start 3 is after end 1"
    );
}

#[tokio::test]
async fn test_fetch_range_rejects_overflowing_range() {
    let client = UpstreamClient::new("https://pypi.org/simple/").unwrap();

    let err = client
        .fetch_range("https://example.invalid/pkg.whl", 0, u64::MAX)
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
        .and(path("/files/pkg.whl"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    let client = simple_client(&server);

    let err = client
        .fetch_range(&format!("{}/files/pkg.whl", server.uri()), 1, 3)
        .await
        .unwrap_err();

    assert_eq!(
        err.to_string(),
        "upstream returned an invalid byte range response: range request returned a non-206 success"
    );
}
