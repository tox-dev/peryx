use futures_util::TryStreamExt as _;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{mount_get, simple_client};
use crate::client::UpstreamError;

#[tokio::test]
async fn test_fetch_project_json_with_metadata() {
    let server = MockServer::start().await;
    mount_get(
        &server,
        "/simple/flask/",
        ResponseTemplate::new(200)
            .insert_header("etag", "\"v1\"")
            .insert_header("x-pypi-last-serial", "123")
            .set_body_raw(b"{\"meta\":{}}".to_vec(), "application/vnd.pypi.simple.v1+json"),
    )
    .await;
    let client = simple_client(&server);

    let response = client.fetch_project("flask", None).await.unwrap();

    assert_eq!(response.status, 200);
    assert_eq!(
        response.content_type.as_deref(),
        Some("application/vnd.pypi.simple.v1+json")
    );
    assert_eq!(response.etag.as_deref(), Some("\"v1\""));
    assert_eq!(response.last_serial, Some(123));
    assert_eq!(&response.body[..], b"{\"meta\":{}}");
}

#[tokio::test]
async fn test_fetch_project_without_optional_cache_headers() {
    let server = MockServer::start().await;
    mount_get(
        &server,
        "/simple/bare/",
        ResponseTemplate::new(200).set_body_raw(b"hi".to_vec(), "text/html"),
    )
    .await;
    let client = simple_client(&server);

    let response = client.fetch_project("bare", None).await.unwrap();

    assert_eq!(response.etag, None);
    assert_eq!(response.last_serial, None);
}

#[tokio::test]
async fn test_fetch_project_rejects_missing_content_type() {
    let server = MockServer::start().await;
    mount_get(
        &server,
        "/simple/bare/",
        ResponseTemplate::new(200).set_body_bytes(b"hi".to_vec()),
    )
    .await;
    let client = simple_client(&server);

    let err = client.fetch_project("bare", None).await.unwrap_err();

    assert!(matches!(&err, UpstreamError::MissingContentType { url } if url.as_str().ends_with("/simple/bare/")));
    assert_eq!(err.status(), None);
    assert_eq!(err.user_message(), "upstream response missed Simple API Content-Type");
}

#[tokio::test]
async fn test_fetch_project_rejects_unsupported_content_type() {
    let server = MockServer::start().await;
    mount_get(
        &server,
        "/simple/bare/",
        ResponseTemplate::new(200).set_body_raw(b"hi".to_vec(), "application/octet-stream"),
    )
    .await;
    let client = simple_client(&server);

    let err = client.fetch_project("bare", None).await.unwrap_err();

    assert!(
        matches!(&err, UpstreamError::UnsupportedContentType { url, content_type } if url.as_str().ends_with("/simple/bare/") && content_type == "application/octet-stream")
    );
    assert_eq!(err.status(), None);
    assert_eq!(
        err.user_message(),
        "upstream returned unsupported Simple API Content-Type"
    );
}

#[tokio::test]
async fn test_fetch_project_invalid_serial_header() {
    let server = MockServer::start().await;
    mount_get(
        &server,
        "/simple/x/",
        ResponseTemplate::new(200)
            .insert_header("x-pypi-last-serial", "not-a-number")
            .set_body_raw(b"{}".to_vec(), "application/vnd.pypi.simple.v1+json"),
    )
    .await;
    let client = simple_client(&server);

    assert_eq!(client.fetch_project("x", None).await.unwrap().last_serial, None);
}

#[tokio::test]
async fn test_head_project_bytes_reads_body() {
    let server = MockServer::start().await;
    mount_get(
        &server,
        "/simple/flask/",
        ResponseTemplate::new(200).set_body_raw(b"{\"meta\":{}}".to_vec(), "application/vnd.pypi.simple.v1+json"),
    )
    .await;
    let client = simple_client(&server);

    let response = client.head_project("flask", None).await.unwrap();

    assert_eq!(&response.bytes().await.unwrap()[..], b"{\"meta\":{}}");
}

#[tokio::test]
async fn test_head_project_into_stream_reads_body() {
    let server = MockServer::start().await;
    mount_get(
        &server,
        "/simple/flask/",
        ResponseTemplate::new(200).set_body_raw(b"{\"meta\":{}}".to_vec(), "application/vnd.pypi.simple.v1+json"),
    )
    .await;
    let client = simple_client(&server);

    let body = client
        .head_project("flask", None)
        .await
        .unwrap()
        .into_stream()
        .try_fold(Vec::new(), |mut body, chunk| async move {
            body.extend_from_slice(&chunk);
            Ok(body)
        })
        .await
        .unwrap();

    assert_eq!(body, b"{\"meta\":{}}");
}

async fn max_age_of(cache_control: Option<&str>) -> Option<i64> {
    let server = MockServer::start().await;
    let mut template = ResponseTemplate::new(200).set_body_raw(b"{}".to_vec(), "application/vnd.pypi.simple.v1+json");
    if let Some(value) = cache_control {
        template = template.insert_header("cache-control", value);
    }
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(template)
        .mount(&server)
        .await;
    let client = simple_client(&server);
    client.fetch_project("flask", None).await.unwrap().max_age
}

#[tokio::test]
async fn test_max_age_parsed_from_cache_control() {
    assert_eq!(max_age_of(Some("public, max-age=600")).await, Some(600));
}

#[tokio::test]
async fn test_s_maxage_beats_max_age() {
    assert_eq!(max_age_of(Some("max-age=600, s-maxage=60")).await, Some(60));
}

#[tokio::test]
async fn test_no_cache_disables_freshness() {
    assert_eq!(max_age_of(Some("no-cache, max-age=600")).await, None);
}

#[tokio::test]
async fn test_no_store_disables_freshness() {
    assert_eq!(max_age_of(Some("no-store")).await, None);
}

#[tokio::test]
async fn test_zero_max_age_counts_as_none() {
    assert_eq!(max_age_of(Some("max-age=0")).await, None);
}

#[tokio::test]
async fn test_absent_cache_control_is_none() {
    assert_eq!(max_age_of(None).await, None);
}
