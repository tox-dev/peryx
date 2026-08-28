use futures_util::TryStreamExt as _;
use peryx_upstream::UpstreamError;
use reqwest::header::{CACHE_CONTROL, HeaderMap, HeaderValue};
use rstest::rstest;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{mount_get, simple_client};
use crate::simple_client::{ResponseCachePolicy, SimpleClientExt as _, response_cache_policy};

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

    assert!(matches!(&err, UpstreamError::InvalidResponse { reason } if reason.ends_with("/simple/bare/")));
    assert_eq!(err.status(), None);
    assert_eq!(err.user_message(), "upstream returned an invalid response");
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
        matches!(&err, UpstreamError::InvalidResponse { reason } if reason.contains("application/octet-stream") && reason.ends_with("/simple/bare/"))
    );
    assert_eq!(err.status(), None);
    assert_eq!(err.user_message(), "upstream returned an invalid response");
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
    assert_eq!(max_age_of(Some("no-cache, max-age=600")).await, Some(0));
}

#[tokio::test]
async fn test_no_store_disables_freshness() {
    assert_eq!(max_age_of(Some("no-store")).await, Some(0));
}

#[tokio::test]
async fn test_zero_max_age_counts_as_none() {
    assert_eq!(max_age_of(Some("max-age=0")).await, Some(0));
}

#[tokio::test]
async fn test_absent_cache_control_is_none() {
    assert_eq!(max_age_of(None).await, None);
}

#[test]
fn test_cache_control_combines_repeated_header_fields() {
    assert_eq!(
        cache_policy(&["max-age=60", "no-store"]),
        ResponseCachePolicy {
            fresh_secs: Some(0),
            must_revalidate: Some(true),
            storable: false,
        }
    );
}

#[rstest]
#[case::max_age_zero_first("max-age=0, max-age=86400")]
#[case::max_age_zero_last("max-age=86400, max-age=0")]
#[case::shared_max_age("s-maxage=0, s-maxage=86400")]
#[case::invalid_first("max-age=invalid, max-age=86400")]
#[case::invalid_last("max-age=86400, max-age=invalid")]
#[test]
fn test_cache_control_duplicate_freshness_is_stale(#[case] value: &str) {
    assert_eq!(cache_policy(&[value]).fresh_secs, Some(0));
}

#[test]
fn test_cache_control_duplicate_freshness_across_fields_is_stale() {
    assert_eq!(cache_policy(&["max-age=0", "max-age=86400"]).fresh_secs, Some(0));
}

#[rstest]
#[case::empty("max-age=")]
#[case::negative("max-age=-1")]
#[case::trailing_text("max-age=60x")]
#[case::empty_quotes("max-age=\"\"")]
#[case::unmatched_quote("max-age=\"60")]
#[case::trailing_quoted_text("max-age=\"60\"x")]
#[case::escaped_comma("max-age=\"6\\,0\"")]
#[case::space_before_equals("max-age =60")]
#[test]
fn test_cache_control_invalid_freshness_is_stale(#[case] value: &str) {
    assert_eq!(cache_policy(&[value]).fresh_secs, Some(0));
}

#[test]
fn test_cache_control_decodes_quoted_pairs() {
    assert_eq!(cache_policy(&[r#"max-age="\6\0""#]).fresh_secs, Some(60));
}

#[test]
fn test_cache_control_ignores_commas_inside_extension_quotes() {
    assert_eq!(
        cache_policy(&[r#"extension="x,max-age=86400", max-age=60"#]).fresh_secs,
        Some(60)
    );
}

#[rstest]
#[case::overflow("max-age=9223372036854775808", i64::MAX)]
#[case::no_cache("no-cache, max-age=9223372036854775808", 0)]
#[test]
fn test_cache_control_saturates_freshness(#[case] value: &str, #[case] expected: i64) {
    assert_eq!(cache_policy(&[value]).fresh_secs, Some(expected));
}

#[test]
fn test_cache_control_non_text_value_is_stale() {
    let mut headers = HeaderMap::new();
    headers.insert(CACHE_CONTROL, HeaderValue::from_bytes(b"max-age=\xff").unwrap());

    assert_eq!(response_cache_policy(&headers).fresh_secs, Some(0));
}

#[rstest]
#[case::must_revalidate(
    "max-age=60, must-revalidate",
    ResponseCachePolicy { fresh_secs: Some(60), must_revalidate: Some(true), storable: true }
)]
#[case::proxy_revalidate(
    "max-age=60, proxy-revalidate",
    ResponseCachePolicy { fresh_secs: Some(60), must_revalidate: Some(true), storable: true }
)]
#[case::shared_max_age(
    "max-age=600, s-maxage=\"60\"",
    ResponseCachePolicy { fresh_secs: Some(60), must_revalidate: Some(true), storable: true }
)]
#[case::private(
    "private, max-age=60",
    ResponseCachePolicy { fresh_secs: Some(60), must_revalidate: Some(false), storable: false }
)]
#[case::qualified_private(
    "private=\"set-cookie\", max-age=60",
    ResponseCachePolicy { fresh_secs: Some(60), must_revalidate: Some(false), storable: false }
)]
#[test]
fn test_response_cache_policy_applies_shared_cache_directives(
    #[case] value: &str,
    #[case] expected: ResponseCachePolicy,
) {
    assert_eq!(cache_policy(&[value]), expected);
}

fn cache_policy(values: &[&str]) -> ResponseCachePolicy {
    let mut headers = HeaderMap::new();
    for value in values {
        headers.append(CACHE_CONTROL, HeaderValue::from_str(value).unwrap());
    }
    response_cache_policy(&headers)
}
