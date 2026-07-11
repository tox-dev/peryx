use peryx_upstream::{Auth, UpstreamClient};
use wiremock::matchers::{header, header_regex, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{mount_get, simple_client};
use crate::simple_client::SimpleClientExt as _;

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
