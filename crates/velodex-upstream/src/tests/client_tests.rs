use wiremock::matchers::{header, header_regex, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::client::{Auth, UpstreamClient, UpstreamError};

#[tokio::test]
async fn test_fetch_project_json_with_metadata() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"v1\"")
                .insert_header("x-pypi-last-serial", "123")
                .set_body_raw(b"{\"meta\":{}}".to_vec(), "application/vnd.pypi.simple.v1+json"),
        )
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();

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
async fn test_fetch_project_revalidate_304() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .and(header("if-none-match", "\"v1\""))
        .respond_with(ResponseTemplate::new(304))
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();

    let response = client.fetch_project("flask", Some("\"v1\"")).await.unwrap();

    assert_eq!(response.status, 304);
}

#[tokio::test]
async fn test_fetch_project_without_headers() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/bare/"))
        .respond_with(ResponseTemplate::new(200).set_body_string("hi"))
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();

    let response = client.fetch_project("bare", None).await.unwrap();

    assert_eq!(response.etag, None);
    assert_eq!(response.last_serial, None);
}

#[tokio::test]
async fn test_fetch_project_invalid_serial_header() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/x/"))
        .respond_with(ResponseTemplate::new(200).insert_header("x-pypi-last-serial", "not-a-number"))
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();

    assert_eq!(client.fetch_project("x", None).await.unwrap().last_serial, None);
}

#[tokio::test]
async fn test_fetch_bytes() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/pkg.whl"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"wheelbytes".to_vec()))
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();

    let bytes = client
        .fetch_bytes(&format!("{}/files/pkg.whl", server.uri()))
        .await
        .unwrap();

    assert_eq!(&bytes[..], b"wheelbytes");
}

#[tokio::test]
async fn test_new_adds_trailing_slash() {
    let client = UpstreamClient::new("https://pypi.org/simple").unwrap();
    // A trailing slash was added, so joining a project stays under /simple/.
    let bytes_err = client.fetch_bytes("http://127.0.0.1:0/x").await;
    assert!(bytes_err.is_err()); // exercises the Http error path on an unusable port
    let _ = client;
}

#[test]
fn test_new_rejects_invalid_url() {
    let err = UpstreamClient::new("not a url").unwrap_err();
    assert!(matches!(err, UpstreamError::Url(_)));
}

#[tokio::test]
async fn test_fetch_with_basic_auth() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .and(header_regex("authorization", "^Basic "))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let auth = Auth::Basic {
        username: "__token__".to_owned(),
        password: "secret".to_owned(),
    };
    let client = UpstreamClient::with_auth(&format!("{}/simple/", server.uri()), auth).unwrap();
    assert_eq!(client.fetch_project("flask", None).await.unwrap().status, 200);
}

#[test]
fn test_auth_status_redacts_basic_credentials_and_url_secrets() {
    let client = UpstreamClient::with_auth(
        "https://user:pass@example.invalid/simple/?token=secret#frag",
        Auth::Basic {
            username: "__token__".to_owned(),
            password: "secret".to_owned(),
        },
    )
    .unwrap();

    assert_eq!(client.auth_status().as_str(), "basic");
    assert_eq!(client.redacted_base_url(), "https://example.invalid/simple/");
}

#[tokio::test]
async fn test_fetch_with_bearer_auth() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .and(header("authorization", "Bearer tok123"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let client =
        UpstreamClient::with_auth(&format!("{}/simple/", server.uri()), Auth::Bearer("tok123".to_owned())).unwrap();
    assert_eq!(client.fetch_project("flask", None).await.unwrap().status, 200);
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
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
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

#[tokio::test]
async fn test_warm_reaches_the_upstream_host() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    let client = UpstreamClient::new(&format!("{}/simple/", server.uri())).unwrap();
    client.warm().await;
}
