use std::sync::Arc;
use std::time::Duration;

use peryx_upstream::{
    Auth, CredentialError, CredentialFailure, CredentialProvider, CredentialRefresh, NamedUpstream, UpstreamClient,
    UpstreamError, UpstreamHealth, UpstreamRouter, UpstreamTls,
};
use rstest::rstest;
use wiremock::matchers::{header, header_regex, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{mount_get, simple_client};
use crate::simple_client::{CachedValidators, SimpleClientExt as _};

fn route(first: &MockServer, second: &MockServer) -> UpstreamRouter {
    UpstreamRouter::new(vec![
        NamedUpstream::new("first", simple_client(first)),
        NamedUpstream::new("second", simple_client(second)),
    ])
    .unwrap()
}

#[tokio::test]
async fn test_fetch_index_json() {
    let server = MockServer::start().await;
    mount_get(
        &server,
        "/simple/",
        ResponseTemplate::new(200).set_body_raw(
            b"{\"meta\":{},\"projects\":[]}".to_vec(),
            "application/vnd.pypi.simple.v1+json",
        ),
    )
    .await;
    let client = simple_client(&server);

    let response = client.fetch_index().await.unwrap();

    assert_eq!(response.status, 200);
    assert_eq!(&response.body[..], b"{\"meta\":{},\"projects\":[]}");
    assert_eq!(response.url.as_str(), format!("{}/simple/", server.uri()));
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
    let client = simple_client(&server);

    let response = client
        .fetch_project(
            "flask",
            CachedValidators {
                etag: Some("\"v1\""),
                ..CachedValidators::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(response.status, 304);
}

#[tokio::test]
async fn test_fetch_project_preserves_retry_after_above_the_wait_budget() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "120"))
        .expect(1)
        .mount(&server)
        .await;
    let client = simple_client(&server);

    let response = client
        .fetch_project("flask", CachedValidators::default())
        .await
        .unwrap();

    assert_eq!((response.status, response.retry_after.as_deref()), (429, Some("120")));
}

#[rstest]
#[case::not_found(404)]
#[case::rate_limited(429)]
#[case::server_error(500)]
#[tokio::test]
async fn test_routed_project_falls_back_on_retryable_status(#[case] status: u16) {
    let first = MockServer::start().await;
    let second = MockServer::start().await;
    mount_get(&first, "/simple/flask/", ResponseTemplate::new(status)).await;
    mount_get(
        &second,
        "/simple/flask/",
        ResponseTemplate::new(200).set_body_raw(b"{}".to_vec(), "application/vnd.pypi.simple.v1+json"),
    )
    .await;

    let route = route(&first, &second);
    let response = route.fetch_project("flask", CachedValidators::default()).await.unwrap();

    assert_eq!(response.status, 200);
    assert_eq!(response.source.as_deref(), Some("second"));
    assert!(response.url.as_str().starts_with(&second.uri()));
    assert_eq!(
        route.sources().map(NamedUpstream::health).collect::<Vec<_>>(),
        if status == 404 {
            vec![UpstreamHealth::Healthy, UpstreamHealth::Healthy]
        } else {
            vec![UpstreamHealth::Unhealthy, UpstreamHealth::Healthy]
        }
    );
}

#[tokio::test]
async fn test_routed_project_falls_back_after_a_transport_error() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let unavailable = listener.local_addr().unwrap();
    let close_connection = tokio::spawn(async move {
        drop(listener.accept().await.unwrap());
    });
    let second = MockServer::start().await;
    mount_get(
        &second,
        "/simple/flask/",
        ResponseTemplate::new(200).set_body_raw(b"{}".to_vec(), "application/vnd.pypi.simple.v1+json"),
    )
    .await;
    let route = UpstreamRouter::new(vec![
        NamedUpstream::new(
            "unavailable",
            UpstreamClient::new(&format!("http://{unavailable}/simple/")).unwrap(),
        ),
        NamedUpstream::new("second", simple_client(&second)),
    ])
    .unwrap();

    let response = route.fetch_project("flask", CachedValidators::default()).await.unwrap();
    close_connection.await.unwrap();

    assert_eq!(response.status, 200);
    assert!(response.url.as_str().starts_with(&second.uri()));
    assert_eq!(
        route.sources().map(NamedUpstream::health).collect::<Vec<_>>(),
        [UpstreamHealth::Unhealthy, UpstreamHealth::Healthy]
    );
}

#[tokio::test(start_paused = true)]
async fn test_routed_project_falls_back_after_deadline() {
    let second = MockServer::start().await;
    mount_get(
        &second,
        "/simple/flask/",
        ResponseTemplate::new(200).set_body_raw(b"{}".to_vec(), "application/vnd.pypi.simple.v1+json"),
    )
    .await;
    let refresh_started = Arc::new(tokio::sync::Notify::new());
    let loader_started = Arc::clone(&refresh_started);
    let credentials = CredentialProvider::refreshing(
        Auth::None,
        CredentialRefresh {
            interval: Duration::ZERO,
            on_unauthorized: false,
            failure: CredentialFailure::Fail,
        },
        move || {
            loader_started.notify_one();
            std::future::pending::<Result<Auth, CredentialError>>()
        },
    );
    let base = "https://deadline.example/simple/";
    let first =
        UpstreamClient::with_credentials_and_tls_for_origin(base, credentials, &UpstreamTls::default(), base, &[])
            .unwrap();
    let route = UpstreamRouter::new(vec![
        NamedUpstream::new("first", first),
        NamedUpstream::new("second", simple_client(&second)),
    ])
    .unwrap();
    let running = route.clone();
    let request = tokio::spawn(async move { running.fetch_project("flask", CachedValidators::default()).await });
    refresh_started.notified().await;
    tokio::time::advance(Duration::from_secs(30)).await;
    tokio::time::resume();
    let response = request.await.unwrap().unwrap();
    tokio::time::pause();
    assert_eq!(
        (
            response.status,
            response.source.as_deref(),
            response.url.to_string(),
            route.sources().map(NamedUpstream::health).collect::<Vec<_>>(),
        ),
        (
            200,
            Some("second"),
            format!("{}/simple/flask/", second.uri()),
            vec![UpstreamHealth::Unhealthy, UpstreamHealth::Healthy],
        )
    );
}

#[tokio::test]
async fn test_routed_project_respects_no_fallback() {
    let first = MockServer::start().await;
    let second = MockServer::start().await;
    mount_get(&first, "/simple/flask/", ResponseTemplate::new(404)).await;
    mount_get(
        &second,
        "/simple/flask/",
        ResponseTemplate::new(200).set_body_raw(b"{}".to_vec(), "application/vnd.pypi.simple.v1+json"),
    )
    .await;

    let response = route(&first, &second)
        .with_fallback(false)
        .fetch_project("flask", CachedValidators::default())
        .await
        .unwrap();

    assert_eq!(response.status, 404);
    assert!(second.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_routed_project_does_not_fall_back_on_an_invalid_response() {
    let first = MockServer::start().await;
    let second = MockServer::start().await;
    mount_get(
        &first,
        "/simple/flask/",
        ResponseTemplate::new(200).set_body_bytes(b"{}".to_vec()),
    )
    .await;
    mount_get(
        &second,
        "/simple/flask/",
        ResponseTemplate::new(200).set_body_raw(b"{}".to_vec(), "application/vnd.pypi.simple.v1+json"),
    )
    .await;

    let route = route(&first, &second);
    let err = route
        .fetch_project("flask", CachedValidators::default())
        .await
        .unwrap_err();

    assert!(matches!(err, UpstreamError::InvalidResponse { .. }));
    assert!(second.received_requests().await.unwrap().is_empty());
    assert_eq!(
        route.sources().map(NamedUpstream::health).collect::<Vec<_>>(),
        [UpstreamHealth::Unhealthy, UpstreamHealth::Configured]
    );
}

#[tokio::test]
async fn test_routed_project_does_not_fall_back_after_a_credential_failure() {
    let first = MockServer::start().await;
    let second = MockServer::start().await;
    mount_get(
        &second,
        "/simple/flask/",
        ResponseTemplate::new(200).set_body_raw(b"{}".to_vec(), "application/vnd.pypi.simple.v1+json"),
    )
    .await;
    let credentials = CredentialProvider::refreshing(
        Auth::Bearer("old".to_owned()),
        CredentialRefresh {
            interval: Duration::ZERO,
            on_unauthorized: true,
            failure: CredentialFailure::Fail,
        },
        || async { Err(CredentialError::new("source unavailable")) },
    );
    let route = UpstreamRouter::new(vec![
        NamedUpstream::new(
            "first",
            UpstreamClient::with_credentials_and_tls_for_origin(
                &format!("{}/simple/", first.uri()),
                credentials,
                &UpstreamTls::default(),
                &first.uri(),
                &[],
            )
            .unwrap(),
        ),
        NamedUpstream::new("second", simple_client(&second)),
    ])
    .unwrap();

    let error = route
        .fetch_project("flask", CachedValidators::default())
        .await
        .unwrap_err();

    assert!(matches!(error, UpstreamError::Credential(_)));
    assert!(second.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn test_routed_project_head_falls_back() {
    let first = MockServer::start().await;
    let second = MockServer::start().await;
    mount_get(&first, "/simple/flask/", ResponseTemplate::new(500)).await;
    mount_get(
        &second,
        "/simple/flask/",
        ResponseTemplate::new(200).set_body_raw(b"{}".to_vec(), "application/vnd.pypi.simple.v1+json"),
    )
    .await;

    let response = route(&first, &second)
        .head_project("flask", CachedValidators::default())
        .await
        .unwrap();

    assert_eq!(response.status, 200);
    assert!(response.url.as_str().starts_with(&second.uri()));
}

#[tokio::test]
async fn test_routed_index_falls_back() {
    let first = MockServer::start().await;
    let second = MockServer::start().await;
    mount_get(&first, "/simple/", ResponseTemplate::new(500)).await;
    mount_get(
        &second,
        "/simple/",
        ResponseTemplate::new(200).set_body_raw(b"{}".to_vec(), "application/vnd.pypi.simple.v1+json"),
    )
    .await;

    let response = route(&first, &second).fetch_index().await.unwrap();

    assert_eq!(response.status, 200);
    assert!(response.url.as_str().starts_with(&second.uri()));
}

#[tokio::test]
async fn test_routed_project_does_not_reuse_an_unattributed_etag() {
    let first = MockServer::start().await;
    let second = MockServer::start().await;
    mount_get(
        &first,
        "/simple/flask/",
        ResponseTemplate::new(200).set_body_raw(b"{}".to_vec(), "application/vnd.pypi.simple.v1+json"),
    )
    .await;

    route(&first, &second)
        .fetch_project(
            "flask",
            CachedValidators {
                source: Some("second"),
                etag: Some("\"other-source\""),
                last_modified: Some("Tue, 01 Sep 2026 00:00:00 GMT"),
            },
        )
        .await
        .unwrap();

    let requests = first.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].headers.contains_key("if-none-match"));
    assert!(!requests[0].headers.contains_key("if-modified-since"));
}

const LAST_MODIFIED: &str = "Tue, 01 Sep 2026 00:00:00 GMT";

async fn mount_conditional_not_modified(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .and(header("if-none-match", "\"v1\""))
        .respond_with(ResponseTemplate::new(304))
        .mount(server)
        .await;
}

fn stored_by_second() -> CachedValidators<'static> {
    CachedValidators {
        source: Some("second"),
        etag: Some("\"v1\""),
        last_modified: None,
    }
}

async fn validators_sent_to(server: &MockServer) -> Vec<Option<String>> {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| {
            request
                .headers
                .get("if-none-match")
                .map(|value| value.to_str().unwrap().to_owned())
        })
        .collect()
}

#[tokio::test]
async fn test_routed_project_revalidates_against_the_answering_source() {
    let first = MockServer::start().await;
    let second = MockServer::start().await;
    mount_get(&first, "/simple/flask/", ResponseTemplate::new(404)).await;
    mount_conditional_not_modified(&second).await;

    let response = route(&first, &second)
        .fetch_project("flask", stored_by_second())
        .await
        .unwrap();

    assert_eq!((response.status, response.source.as_deref()), (304, Some("second")));
    assert!(response.body.is_empty());
    assert_eq!(validators_sent_to(&second).await, [Some("\"v1\"".to_owned())]);
    assert_eq!(validators_sent_to(&first).await, [None]);
}

#[tokio::test]
async fn test_routed_project_head_revalidates_against_the_answering_source() {
    let first = MockServer::start().await;
    let second = MockServer::start().await;
    mount_get(&first, "/simple/flask/", ResponseTemplate::new(404)).await;
    mount_conditional_not_modified(&second).await;

    let response = route(&first, &second)
        .head_project("flask", stored_by_second())
        .await
        .unwrap();

    assert_eq!((response.status, response.source.as_deref()), (304, Some("second")));
    assert_eq!(validators_sent_to(&second).await, [Some("\"v1\"".to_owned())]);
    assert_eq!(validators_sent_to(&first).await, [None]);
}

#[tokio::test]
async fn test_project_revalidates_with_last_modified_alone() {
    let server = MockServer::start().await;
    mount_get(&server, "/simple/flask/", ResponseTemplate::new(304)).await;

    let response = simple_client(&server)
        .head_project(
            "flask",
            CachedValidators {
                last_modified: Some(LAST_MODIFIED),
                ..CachedValidators::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(response.status, 304);
    let sent = server.received_requests().await.unwrap();
    assert_eq!(sent[0].headers["if-modified-since"], LAST_MODIFIED);
}

#[tokio::test]
async fn test_fetch_with_basic_auth() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .and(header_regex("authorization", "^Basic "))
        .respond_with(ResponseTemplate::new(200).set_body_raw(b"{}".to_vec(), "application/vnd.pypi.simple.v1+json"))
        .mount(&server)
        .await;
    let auth = Auth::Basic {
        username: "__token__".to_owned(),
        password: "secret".to_owned(),
    };
    let client = UpstreamClient::with_auth(&format!("{}/simple/", server.uri()), auth).unwrap();
    assert_eq!(
        client
            .fetch_project("flask", CachedValidators::default())
            .await
            .unwrap()
            .status,
        200
    );
}

#[tokio::test]
async fn test_fetch_project_preserves_basic_auth_on_same_host_redirect() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/source/"))
        .and(header_regex("authorization", "^Basic "))
        .respond_with(ResponseTemplate::new(302).insert_header("location", format!("{}/simple/flask/", server.uri())))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .and(header_regex("authorization", "^Basic "))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(b"{\"meta\":{}}".to_vec(), "application/vnd.pypi.simple.v1+json"),
        )
        .mount(&server)
        .await;
    let client = UpstreamClient::with_auth(
        &format!("{}/simple/", server.uri()),
        Auth::Basic {
            username: "__token__".to_owned(),
            password: "secret".to_owned(),
        },
    )
    .unwrap();

    assert_eq!(
        client
            .fetch_project("source", CachedValidators::default())
            .await
            .unwrap()
            .status,
        200
    );
}

#[tokio::test]
async fn test_fetch_with_bearer_auth() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/simple/flask/"))
        .and(header("authorization", "Bearer tok123"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(b"{}".to_vec(), "application/vnd.pypi.simple.v1+json"))
        .mount(&server)
        .await;
    let client =
        UpstreamClient::with_auth(&format!("{}/simple/", server.uri()), Auth::Bearer("tok123".to_owned())).unwrap();
    assert_eq!(
        client
            .fetch_project("flask", CachedValidators::default())
            .await
            .unwrap()
            .status,
        200
    );
}

#[tokio::test]
async fn test_upstream_protocol_trait_dispatches_to_the_client() {
    use crate::simple_client::UpstreamProtocol;
    let server = MockServer::start().await;
    for p in ["/simple/", "/simple/flask/"] {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(b"{}".to_vec(), "application/vnd.pypi.simple.v1+json"),
            )
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/file.whl"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(b"wheel".to_vec(), "application/octet-stream"))
        .mount(&server)
        .await;
    let client = simple_client(&server);
    UpstreamProtocol::fetch_index(&client).await.unwrap();
    UpstreamProtocol::fetch_project(&client, "flask", CachedValidators::default())
        .await
        .unwrap();
    let bytes = UpstreamProtocol::fetch_bytes(&client, &format!("{}/file.whl", server.uri()))
        .await
        .unwrap();
    assert_eq!(&bytes[..], b"wheel");
}
