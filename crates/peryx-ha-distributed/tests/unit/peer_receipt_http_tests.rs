use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use peryx_storage::blob::{BlobStorage, Digest};
use tower::ServiceExt as _;

use crate::peer::TransportError;
use crate::peer_receipt::ReceiptSource;
use crate::peer_receipt_http::{HttpReceiptError, HttpReceiptSource, ReceiptReply, receipt_router};

const ROUTE: &str = "/+replication/v1/receipts/sha256/{digest}";
const TOKEN: &str = "secret";

fn digest() -> Digest {
    Digest::of(b"artifact")
}

fn source(url: &str, node: &str) -> HttpReceiptSource {
    HttpReceiptSource::new(url, node, TOKEN, Duration::from_secs(5)).unwrap()
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

/// A receipt endpoint that answers every digest with one preset response, for driving the client's
/// status handling without a store.
fn serving(response: impl Fn() -> Response + Clone + Send + Sync + 'static) -> Router {
    Router::new().route(
        ROUTE,
        get(move || {
            let response = response.clone();
            async move { response() }
        }),
    )
}

async fn store_with(bytes: &[u8]) -> (tempfile::TempDir, BlobStorage, Digest) {
    let dir = tempfile::tempdir().unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let digest = blobs.put_bytes(bytes).await.unwrap();
    (dir, blobs, digest)
}

#[test]
fn test_new_rejects_an_empty_token() {
    let error = HttpReceiptSource::new("http://peer/", "b", "", Duration::from_secs(1)).unwrap_err();

    assert!(matches!(error, HttpReceiptError::EmptyToken));
}

#[test]
fn test_new_rejects_an_empty_node() {
    let error = HttpReceiptSource::new("http://peer/", "", TOKEN, Duration::from_secs(1)).unwrap_err();

    assert!(matches!(error, HttpReceiptError::EmptyNode));
}

#[test]
fn test_new_rejects_an_unparseable_url() {
    let error = HttpReceiptSource::new("not a url", "b", TOKEN, Duration::from_secs(1)).unwrap_err();

    assert!(matches!(error, HttpReceiptError::InvalidBase(_)));
}

#[test]
fn test_new_rejects_a_non_http_scheme() {
    let error = HttpReceiptSource::new("ftp://peer/", "b", TOKEN, Duration::from_secs(1)).unwrap_err();

    assert!(matches!(error, HttpReceiptError::InvalidBase(_)));
}

#[test]
fn test_debug_redacts_the_token() {
    let rendered = format!("{:?}", source("http://peer.example/", "b"));

    assert!(rendered.contains("<redacted>"), "{rendered}");
    assert!(rendered.contains('b'), "the node is not a secret: {rendered}");
    assert!(!rendered.contains(TOKEN), "token leaked: {rendered}");
}

#[test]
fn test_node_reports_the_configured_peer() {
    assert_eq!(source("http://peer.example/", "east-2").node(), "east-2");
}

#[tokio::test]
async fn test_fetch_parses_a_receipt_from_a_nested_base() {
    let router = Router::new().nest("/mirror", serving(|| Json(ReceiptReply { size: 7 }).into_response()));
    let server = TestServer::start(router).await;
    let source = source(&format!("{}mirror", server.url), "b");

    let receipt = source.fetch_receipt(&digest()).await.unwrap().unwrap();

    assert_eq!(receipt.node, "b");
    assert_eq!(receipt.digest, digest());
    assert_eq!(receipt.size, 7);
}

#[tokio::test]
async fn test_fetch_maps_a_missing_blob_to_none() {
    let server = TestServer::start(serving(|| StatusCode::NOT_FOUND.into_response())).await;

    let receipt = source(&server.url, "b").fetch_receipt(&digest()).await.unwrap();

    assert_eq!(receipt, None);
}

#[tokio::test]
async fn test_fetch_maps_unauthorized_to_unauthenticated() {
    let server = TestServer::start(serving(|| StatusCode::UNAUTHORIZED.into_response())).await;

    let error = source(&server.url, "b").fetch_receipt(&digest()).await.unwrap_err();

    assert_eq!(error, TransportError::Unauthenticated);
}

#[tokio::test]
async fn test_fetch_maps_a_transient_server_error_to_retryable() {
    let server = TestServer::start(serving(|| StatusCode::BAD_GATEWAY.into_response())).await;

    let error = source(&server.url, "b").fetch_receipt(&digest()).await.unwrap_err();

    assert_eq!(error, TransportError::ServerError { status: 502 });
    assert!(error.is_retryable());
}

#[tokio::test]
async fn test_fetch_keeps_not_implemented_terminal() {
    let server = TestServer::start(serving(|| StatusCode::NOT_IMPLEMENTED.into_response())).await;

    let error = source(&server.url, "b").fetch_receipt(&digest()).await.unwrap_err();

    assert_eq!(error, TransportError::BadStatus { status: 501 });
}

#[tokio::test]
async fn test_fetch_maps_an_unexpected_status_to_bad_status() {
    let server = TestServer::start(serving(|| StatusCode::IM_A_TEAPOT.into_response())).await;

    let error = source(&server.url, "b").fetch_receipt(&digest()).await.unwrap_err();

    assert_eq!(error, TransportError::BadStatus { status: 418 });
}

#[tokio::test]
async fn test_fetch_rejects_a_malformed_reply() {
    let server = TestServer::start(serving(|| (StatusCode::OK, "not json").into_response())).await;

    let error = source(&server.url, "b").fetch_receipt(&digest()).await.unwrap_err();

    assert_eq!(error, TransportError::Malformed);
}

#[tokio::test]
async fn test_fetch_rejects_an_oversized_reply() {
    let server = TestServer::start(serving(|| {
        let body = "x".repeat(8192);
        let stream =
            futures_util::stream::once(async move { Ok::<_, std::convert::Infallible>(bytes::Bytes::from(body)) });
        (StatusCode::OK, Body::from_stream(stream)).into_response()
    }))
    .await;

    let error = source(&server.url, "b").fetch_receipt(&digest()).await.unwrap_err();

    assert_eq!(error, TransportError::Malformed);
}

#[tokio::test]
async fn test_fetch_maps_a_dead_peer_to_disconnected() {
    // Port 1 refuses immediately: a connection loss that is not a deadline.
    let error = source("http://127.0.0.1:1/", "b")
        .fetch_receipt(&digest())
        .await
        .unwrap_err();

    assert_eq!(error, TransportError::Disconnected);
}

#[tokio::test]
async fn test_fetch_maps_a_deadline_to_timeout() {
    let server = TestServer::start(serving(|| {
        let stream = futures_util::stream::pending::<Result<bytes::Bytes, std::convert::Infallible>>();
        Body::from_stream(stream).into_response()
    }))
    .await;
    let slow = HttpReceiptSource::new(&server.url, "b", TOKEN, Duration::from_millis(150)).unwrap();

    let error = slow.fetch_receipt(&digest()).await.unwrap_err();

    assert_eq!(error, TransportError::Timeout);
}

#[test]
fn test_router_rejects_an_empty_token() {
    let dir = tempfile::tempdir().unwrap();
    let error = receipt_router("", BlobStorage::filesystem(dir.path().join("blobs"))).unwrap_err();

    assert!(matches!(error, HttpReceiptError::EmptyToken));
}

#[tokio::test]
async fn test_endpoint_serves_a_held_receipt_end_to_end() {
    let (_dir, blobs, digest) = store_with(b"artifact bytes").await;
    let server = TestServer::start(receipt_router(TOKEN, blobs).unwrap()).await;

    let receipt = source(&server.url, "b").fetch_receipt(&digest).await.unwrap().unwrap();

    assert_eq!(receipt.digest, digest);
    assert_eq!(receipt.size, "artifact bytes".len() as u64);
}

#[tokio::test]
async fn test_endpoint_reports_a_blob_it_does_not_hold_as_absent() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let server = TestServer::start(receipt_router(TOKEN, blobs).unwrap()).await;

    let receipt = source(&server.url, "b").fetch_receipt(&digest()).await.unwrap();

    assert_eq!(receipt, None);
}

#[tokio::test]
async fn test_endpoint_rejects_a_bad_credential() {
    let (_dir, blobs, digest) = store_with(b"artifact bytes").await;
    let server = TestServer::start(receipt_router(TOKEN, blobs).unwrap()).await;
    let wrong = HttpReceiptSource::new(&server.url, "b", "wrong", Duration::from_secs(5)).unwrap();

    let error = wrong.fetch_receipt(&digest).await.unwrap_err();

    assert_eq!(error, TransportError::Unauthenticated);
}

#[tokio::test]
async fn test_endpoint_rejects_an_unparseable_digest() {
    let dir = tempfile::tempdir().unwrap();
    let router = receipt_router(TOKEN, BlobStorage::filesystem(dir.path().join("blobs"))).unwrap();

    let response = router
        .oneshot(
            Request::get("/+replication/v1/receipts/sha256/not-a-digest")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[cfg(unix)]
#[tokio::test]
async fn test_endpoint_reports_a_store_read_failure() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().unwrap();
    let blobs_dir = dir.path().join("blobs");
    let blobs = BlobStorage::filesystem(blobs_dir.clone());
    let digest = blobs.put_bytes(b"payload").await.unwrap();
    let router = receipt_router(TOKEN, blobs).unwrap();
    // Drop search permission on the store root so the durability lookup fails to stat.
    std::fs::set_permissions(&blobs_dir, std::fs::Permissions::from_mode(0o000)).unwrap();

    let response = router
        .oneshot(
            Request::get(format!("/+replication/v1/receipts/sha256/{}", digest.as_str()))
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    std::fs::set_permissions(&blobs_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}
