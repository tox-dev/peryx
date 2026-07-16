use peryx_upstream::{Auth, NamedUpstream, UpstreamClient, UpstreamError, UpstreamHealth, UpstreamRouter};
use rstest::rstest;
use wiremock::matchers::{header, header_regex, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{mount_get, simple_client};
use crate::simple_client::SimpleClientExt as _;

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

    let response = client.fetch_project("flask", Some("\"v1\"")).await.unwrap();

    assert_eq!(response.status, 304);
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
    let response = route.fetch_project("flask", None).await.unwrap();

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
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let unavailable = listener.local_addr().unwrap();
    drop(listener);
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

    let response = route.fetch_project("flask", None).await.unwrap();

    assert_eq!(response.status, 200);
    assert!(response.url.as_str().starts_with(&second.uri()));
    assert_eq!(
        route.sources().map(NamedUpstream::health).collect::<Vec<_>>(),
        [UpstreamHealth::Unhealthy, UpstreamHealth::Healthy]
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
        .fetch_project("flask", None)
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
    let err = route.fetch_project("flask", None).await.unwrap_err();

    assert!(matches!(err, UpstreamError::MissingContentType { .. }));
    assert!(second.received_requests().await.unwrap().is_empty());
    assert_eq!(
        route.sources().map(NamedUpstream::health).collect::<Vec<_>>(),
        [UpstreamHealth::Unhealthy, UpstreamHealth::Configured]
    );
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

    let response = route(&first, &second).head_project("flask", None).await.unwrap();

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
        .fetch_project("flask", Some("\"other-source\""))
        .await
        .unwrap();

    let requests = first.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].headers.contains_key("if-none-match"));
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
    assert_eq!(client.fetch_project("flask", None).await.unwrap().status, 200);
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

    assert_eq!(client.fetch_project("source", None).await.unwrap().status, 200);
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
    assert_eq!(client.fetch_project("flask", None).await.unwrap().status, 200);
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
    UpstreamProtocol::fetch_project(&client, "flask", None).await.unwrap();
    let bytes = UpstreamProtocol::fetch_bytes(&client, &format!("{}/file.whl", server.uri()))
        .await
        .unwrap();
    assert_eq!(&bytes[..], b"wheel");
}
