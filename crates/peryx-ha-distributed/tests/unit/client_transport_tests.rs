use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use axum::routing::get;

use crate::client_transport::{
    HttpClientConfigError, HttpClientError, HttpClientTransport, ReplicationStatus, classify_status, replication_error,
    require_replication_success,
};
use crate::peer::TransportError;
use crate::support::TestServer;

const TOKEN: &str = "secret";

fn make_transport(base: &str) -> HttpClientTransport {
    HttpClientTransport::new(base, TOKEN, Duration::from_secs(1)).unwrap()
}

#[test]
fn test_configuration_and_debug_contract() {
    assert_eq!(
        HttpClientTransport::new("http://peer/", "", Duration::from_secs(1)).unwrap_err(),
        HttpClientConfigError::EmptyToken
    );
    for base in ["not a url", "ftp://peer/"] {
        assert!(matches!(
            HttpClientTransport::new(base, TOKEN, Duration::from_secs(1)).unwrap_err(),
            HttpClientConfigError::InvalidBase(_)
        ));
    }
    let rendered = format!(
        "{:?}",
        make_transport("https://peer.test/root?query=hidden-query#hidden-fragment")
    );
    assert!(rendered.contains("Domain(\"peer.test\")"), "{rendered}");
    assert!(rendered.contains("path: \"/root/\""), "{rendered}");
    assert!(rendered.contains("<redacted>"), "{rendered}");
    assert!(!rendered.contains(TOKEN), "{rendered}");
    assert!(!rendered.contains("hidden-query"), "{rendered}");
    assert!(!rendered.contains("hidden-fragment"), "{rendered}");
}

#[test]
fn test_status_contract() {
    assert_eq!(classify_status(StatusCode::OK), ReplicationStatus::Success);
    assert_eq!(
        classify_status(StatusCode::UNAUTHORIZED),
        ReplicationStatus::Unauthenticated
    );
    assert_eq!(classify_status(StatusCode::NOT_FOUND), ReplicationStatus::NotFound);
    assert_eq!(
        classify_status(StatusCode::INTERNAL_SERVER_ERROR),
        ReplicationStatus::ServerError(500)
    );
    assert_eq!(
        classify_status(StatusCode::NOT_IMPLEMENTED),
        ReplicationStatus::BadStatus(501)
    );
    assert_eq!(
        classify_status(StatusCode::BAD_REQUEST),
        ReplicationStatus::BadStatus(400)
    );
    assert_eq!(require_replication_success(StatusCode::OK), Ok(()));
    assert_eq!(
        require_replication_success(StatusCode::UNAUTHORIZED),
        Err(TransportError::Unauthenticated)
    );
    assert_eq!(
        require_replication_success(StatusCode::NOT_FOUND),
        Err(TransportError::BadStatus { status: 404 })
    );
    assert_eq!(
        require_replication_success(StatusCode::SERVICE_UNAVAILABLE),
        Err(TransportError::ServerError { status: 503 })
    );
    assert_eq!(
        require_replication_success(StatusCode::NOT_IMPLEMENTED),
        Err(TransportError::BadStatus { status: 501 })
    );
    assert_eq!(replication_error(HttpClientError::Timeout), TransportError::Timeout);
    assert_eq!(
        replication_error(HttpClientError::Disconnected),
        TransportError::Disconnected
    );
    assert_eq!(
        replication_error(HttpClientError::BodyTooLarge { limit: 5, actual: 6 }),
        TransportError::FrameTooLarge { limit: 5, actual: 6 }
    );
}

#[tokio::test]
async fn test_request_contract() {
    async fn handle(headers: HeaderMap) -> Response {
        if headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok()) != Some("Bearer secret") {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        Response::new(Body::from("reply"))
    }

    use axum::response::IntoResponse as _;

    let server = TestServer::start(Router::new().route("/root/reply", get(handle))).await;
    assert_eq!(
        reqwest::get(format!("{}root/reply", server.url))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED,
    );
    let transport = make_transport(&format!("{}root", server.url));
    let response = transport
        .send(transport.get(transport.endpoint("reply")))
        .await
        .unwrap();

    assert_eq!(classify_status(response.status()), ReplicationStatus::Success);
    assert_eq!(transport.read_bounded(response, 5, true).await.unwrap(), b"reply");
}

#[tokio::test]
async fn test_body_limit_contract() {
    let server = TestServer::start(Router::new().route("/fixed", get(|| async { "123456" }))).await;
    let transport = make_transport(&server.url);
    let response = transport
        .send(transport.get(transport.endpoint("fixed")))
        .await
        .unwrap();
    assert_eq!(
        transport.read_bounded(response, 5, true).await.unwrap_err(),
        HttpClientError::BodyTooLarge { limit: 5, actual: 6 }
    );

    let server = TestServer::start(Router::new().route(
        "/chunked",
        get(|| async {
            Response::new(Body::from_stream(futures_util::stream::iter([
                Ok::<_, std::io::Error>("123456"),
            ])))
        }),
    ))
    .await;
    let transport = make_transport(&server.url);
    let response = transport
        .send(transport.get(transport.endpoint("chunked")))
        .await
        .unwrap();
    assert_eq!(
        transport.read_bounded(response, 5, false).await.unwrap_err(),
        HttpClientError::BodyTooLarge { limit: 5, actual: 6 }
    );

    let server = TestServer::start(Router::new().route("/small", get(|| async { "123456" }))).await;
    let transport = make_transport(&server.url);
    let response = transport
        .send(transport.get(transport.endpoint("small")))
        .await
        .unwrap();
    assert_eq!(
        transport.read_small_body(response, 5).await.unwrap_err(),
        TransportError::Malformed
    );
}

#[tokio::test]
async fn test_disconnect_contract() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0_u8; 1024];
        assert_ne!(stream.read(&mut request).await.unwrap(), 0);
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\n12")
            .await
            .unwrap();
    });
    let transport = make_transport(&format!("http://{address}/"));
    let response = transport
        .send(transport.get(transport.endpoint("reply")))
        .await
        .unwrap();

    assert_eq!(
        transport.read_small_body(response, 10).await.unwrap_err(),
        TransportError::Disconnected
    );
    task.await.unwrap();
}

#[tokio::test]
async fn test_deadline_contract() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let (_connection, _) = listener.accept().await.unwrap();
        accepted_tx.send(()).unwrap();
        release_rx.await.unwrap();
    });
    let transport = HttpClientTransport::new(&format!("http://{address}/"), TOKEN, Duration::from_millis(50)).unwrap();
    let request = tokio::spawn(async move { transport.send(transport.get(transport.endpoint("reply"))).await });
    accepted_rx.await.unwrap();

    assert_eq!(request.await.unwrap().unwrap_err(), HttpClientError::Timeout);
    release_tx.send(()).unwrap();
    task.await.unwrap();
}
