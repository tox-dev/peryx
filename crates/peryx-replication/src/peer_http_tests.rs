use std::num::{NonZeroU64, NonZeroUsize};
use std::time::Duration;

use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};

use crate::peer::{BatchRequest, PeerTransport, TransferLimits, TransportError};
use crate::peer_http::{HttpPeerError, HttpPeerTransport};
use crate::protocol::{Change, ChangePage, PROTOCOL_VERSION};

const CHANGES_ROUTE: &str = "/+replication/v1/changes";
const TOKEN: &str = "secret";

fn change(serial: u64) -> Change {
    Change {
        serial,
        event: serial.to_le_bytes().to_vec(),
        metadata: Vec::new(),
        blobs: Vec::new(),
    }
}

fn sample_page() -> ChangePage {
    ChangePage {
        version: PROTOCOL_VERSION,
        source: "primary-a".to_owned(),
        after: 0,
        current_serial: 2,
        changes: vec![change(1), change(2)],
    }
}

fn limits(max_operations: usize, max_encoded_bytes: u64) -> TransferLimits {
    TransferLimits {
        max_operations: NonZeroUsize::new(max_operations).unwrap(),
        max_encoded_bytes: NonZeroU64::new(max_encoded_bytes).unwrap(),
    }
}

fn request(after: u64, max_operations: usize) -> BatchRequest {
    BatchRequest {
        after,
        max_operations: NonZeroUsize::new(max_operations).unwrap(),
    }
}

struct TestServer {
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl TestServer {
    async fn start(router: Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self {
            url: format!("http://{address}/"),
            task,
        }
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn transport(base: &str, transfer: TransferLimits) -> HttpPeerTransport {
    HttpPeerTransport::new(base, TOKEN, transfer, Duration::from_secs(5)).unwrap()
}

#[test]
fn test_new_rejects_an_empty_token() {
    let error =
        HttpPeerTransport::new("http://peer.example/", "", limits(8, 4096), Duration::from_secs(5)).unwrap_err();

    assert!(matches!(error, HttpPeerError::EmptyToken));
}

#[test]
fn test_debug_redacts_the_token() {
    let transport = transport("http://peer.example/", limits(8, 4096));

    let rendered = format!("{transport:?}");

    assert!(rendered.contains("<redacted>"), "{rendered}");
    assert!(!rendered.contains(TOKEN), "token leaked: {rendered}");
}

#[test]
fn test_new_rejects_an_unparseable_url() {
    let error = HttpPeerTransport::new("not a url", TOKEN, limits(8, 4096), Duration::from_secs(5)).unwrap_err();

    assert!(matches!(error, HttpPeerError::InvalidBase(_)));
}

#[test]
fn test_new_rejects_a_non_http_scheme() {
    let error =
        HttpPeerTransport::new("ftp://peer.example/", TOKEN, limits(8, 4096), Duration::from_secs(5)).unwrap_err();

    assert!(matches!(error, HttpPeerError::InvalidBase(_)));
}

#[tokio::test]
async fn test_fetch_rejects_a_request_over_the_operation_bound() {
    let transport = transport("http://127.0.0.1:1/", limits(2, 4096));

    let error = transport.fetch_batch(request(0, 5)).await.unwrap_err();

    assert_eq!(error, TransportError::TooManyOperations { limit: 2, actual: 5 });
}

#[tokio::test]
async fn test_fetch_parses_a_valid_batch_from_a_nested_base() {
    let router = Router::new().nest(
        "/mirror",
        Router::new().route(CHANGES_ROUTE, get(|| async { Json(sample_page()) })),
    );
    let server = TestServer::start(router).await;
    let transport = transport(&format!("{}mirror", server.url), limits(256, 4 << 20));

    let frame = transport.fetch_batch(request(0, 10)).await.unwrap();

    assert_eq!(frame.frontier(), ("primary-a", 2));
    assert_eq!(frame.page().changes.len(), 2);
    assert_eq!(frame.page().changes[0].serial, 1);
}

#[tokio::test]
async fn test_fetch_maps_an_auth_reject_to_unauthenticated() {
    let router = Router::new().route(CHANGES_ROUTE, get(|| async { StatusCode::UNAUTHORIZED }));
    let server = TestServer::start(router).await;
    let transport = transport(&server.url, limits(256, 4 << 20));

    let error = transport.fetch_batch(request(0, 10)).await.unwrap_err();

    assert_eq!(error, TransportError::Unauthenticated);
}

#[tokio::test]
async fn test_fetch_maps_a_transient_server_error_to_a_retryable_arm() {
    let router = Router::new().route(CHANGES_ROUTE, get(|| async { StatusCode::SERVICE_UNAVAILABLE }));
    let server = TestServer::start(router).await;
    let transport = transport(&server.url, limits(256, 4 << 20));

    let error = transport.fetch_batch(request(0, 10)).await.unwrap_err();

    assert_eq!(error, TransportError::ServerError { status: 503 });
    assert!(error.is_retryable());
}

#[tokio::test]
async fn test_fetch_maps_a_client_error_to_terminal_bad_status() {
    let router = Router::new().route(CHANGES_ROUTE, get(|| async { StatusCode::NOT_FOUND }));
    let server = TestServer::start(router).await;
    let transport = transport(&server.url, limits(256, 4 << 20));

    let error = transport.fetch_batch(request(0, 10)).await.unwrap_err();

    assert_eq!(error, TransportError::BadStatus { status: 404 });
    assert!(!error.is_retryable());
}

#[tokio::test]
async fn test_fetch_maps_not_implemented_to_terminal_bad_status() {
    let router = Router::new().route(CHANGES_ROUTE, get(|| async { StatusCode::NOT_IMPLEMENTED }));
    let server = TestServer::start(router).await;
    let transport = transport(&server.url, limits(256, 4 << 20));

    let error = transport.fetch_batch(request(0, 10)).await.unwrap_err();

    assert_eq!(error, TransportError::BadStatus { status: 501 });
    assert!(!error.is_retryable());
}

#[tokio::test]
async fn test_fetch_rejects_an_oversized_frame_before_reading_it() {
    let router = Router::new().route(CHANGES_ROUTE, get(|| async { Json(sample_page()) }));
    let server = TestServer::start(router).await;
    let transport = transport(&server.url, limits(256, 1));

    let error = transport.fetch_batch(request(0, 10)).await.unwrap_err();

    let TransportError::FrameTooLarge { limit, actual } = error else {
        panic!("expected a frame-too-large rejection, got {error:?}");
    };
    assert_eq!(limit, 1);
    assert!(actual > 1);
}

#[tokio::test]
async fn test_fetch_maps_a_non_page_body_to_malformed() {
    let router = Router::new().route(CHANGES_ROUTE, get(|| async { (StatusCode::OK, "not a change page") }));
    let server = TestServer::start(router).await;
    let transport = transport(&server.url, limits(256, 4 << 20));

    let error = transport.fetch_batch(request(0, 10)).await.unwrap_err();

    assert_eq!(error, TransportError::Malformed);
}

#[tokio::test]
async fn test_fetch_maps_a_refused_connection_to_disconnected() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let transport = transport(&format!("http://{address}/"), limits(256, 4 << 20));

    let error = transport.fetch_batch(request(0, 10)).await.unwrap_err();

    assert_eq!(error, TransportError::Disconnected);
}

#[tokio::test]
async fn test_fetch_maps_a_truncated_body_to_disconnected() {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await;
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n")
                .await;
        }
    });
    let transport = transport(&format!("http://{address}/"), limits(256, 4 << 20));

    let error = transport.fetch_batch(request(0, 10)).await.unwrap_err();

    assert_eq!(error, TransportError::Disconnected);
    task.abort();
}

#[tokio::test]
async fn test_fetch_maps_a_deadline_to_timeout() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let _held = stream;
            std::future::pending::<()>().await;
        }
    });
    let transport = HttpPeerTransport::new(
        &format!("http://{address}/"),
        TOKEN,
        limits(256, 4 << 20),
        Duration::from_millis(200),
    )
    .unwrap();

    let error = transport.fetch_batch(request(0, 10)).await.unwrap_err();

    assert_eq!(error, TransportError::Timeout);
    task.abort();
}
