use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::IntoResponse;
use peryx_storage::blob::{BlobStorage, Digest};
use tower::ServiceExt as _;

use crate::peer::TransportError;
use crate::peer_receipt::ReceiptSource;
use crate::peer_receipt_http::{HttpReceiptError, HttpReceiptSource, ReceiptReply, receipt_router};
use crate::support::{TestServer, http_contract};

const ROUTE: &str = "/+replication/v1/receipts/sha256/{digest}";
const TOKEN: &str = "secret";

fn digest() -> Digest {
    Digest::of(b"artifact")
}

fn source(url: &str, node: &str) -> HttpReceiptSource {
    HttpReceiptSource::new(url, node, TOKEN, Duration::from_secs(5)).unwrap()
}

async fn store_with(bytes: &[u8]) -> (tempfile::TempDir, BlobStorage, Digest) {
    let dir = tempfile::tempdir().unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let digest = blobs.put_bytes(bytes).await.unwrap();
    (dir, blobs, digest)
}

#[test]
fn test_configuration_contract() {
    http_contract::assert_configuration(
        |base, token| HttpReceiptSource::new(base, "b", token, Duration::from_secs(1)).map(|_| ()),
        |error| matches!(error, HttpReceiptError::EmptyToken),
        |error| matches!(error, HttpReceiptError::InvalidBase(_)),
    );
}

#[test]
fn test_new_rejects_an_empty_node() {
    let error = HttpReceiptSource::new("http://peer/", "", TOKEN, Duration::from_secs(1)).unwrap_err();

    assert!(matches!(error, HttpReceiptError::EmptyNode));
}

#[test]
fn test_node_reports_the_configured_peer() {
    assert_eq!(source("http://peer.example/", "east-2").node(), "east-2");
}

#[test]
fn test_debug_names_the_peer_without_the_token() {
    http_contract::assert_redacted(
        &source("http://peer.example/root", "east-2"),
        TOKEN,
        &["HttpReceiptSource", "east-2"],
    );
}

#[tokio::test]
async fn test_fetch_parses_a_receipt_from_a_nested_base() {
    let receipt = http_contract::run_nested(
        http_contract::fixed_get(ROUTE, || Json(ReceiptReply { size: 7 }).into_response()),
        |base| async move { source(&base, "b").fetch_receipt(&digest()).await.unwrap().unwrap() },
    )
    .await;

    assert_eq!(receipt.node, "b");
    assert_eq!(receipt.digest, digest());
    assert_eq!(receipt.size, 7);
}

#[tokio::test]
async fn test_fetch_maps_a_missing_blob_to_none() {
    http_contract::assert_mapping(
        http_contract::fixed_get(ROUTE, || StatusCode::NOT_FOUND.into_response()),
        |base| async move { source(&base, "b").fetch_receipt(&digest()).await.unwrap() },
        None,
    )
    .await;
}

#[tokio::test]
async fn test_fetch_rejects_a_malformed_reply() {
    http_contract::assert_mapping(
        http_contract::fixed_get(ROUTE, || (StatusCode::OK, "not json").into_response()),
        |base| async move { source(&base, "b").fetch_receipt(&digest()).await },
        Err(TransportError::Malformed),
    )
    .await;
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
