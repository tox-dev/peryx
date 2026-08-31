use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::IntoResponse;
use peryx_storage::blob::{BlobStorage, Digest};
use tower::ServiceExt as _;

use crate::dc_ack::Deadline;
use crate::filesystem_ack::FilesystemAck;
use crate::peer::TransportError;
use crate::peer_receipt::{ReceiptRequest, ReceiptSource, gather_receipts};
use crate::peer_receipt_http::{HttpReceiptError, HttpReceiptSource, ReceiptReply, receipt_router};
use crate::readiness::DurabilityPolicy;
use crate::receipt_quorum::ReceiptAck;
use crate::support::{TestServer, http_contract};

const ROUTE: &str = "/+replication/v1/receipts/sha256/{digest}";
const TOKEN: &str = "secret";
const BYTES: &[u8] = b"artifact bytes";

fn digest() -> Digest {
    Digest::of(b"artifact")
}

fn request(digest: &Digest, size: u64) -> ReceiptRequest<'_> {
    ReceiptRequest { digest, size }
}

fn source(url: &str, node: &str) -> HttpReceiptSource {
    HttpReceiptSource::new(url, node, TOKEN, Duration::from_secs(5)).unwrap()
}

fn reply(node: &str, digest: &Digest, size: u64) -> ReceiptReply {
    ReceiptReply {
        node: node.to_owned(),
        digest: digest.as_str().to_owned(),
        size,
    }
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
        http_contract::fixed_get(ROUTE, || Json(reply("b", &digest(), 7)).into_response()),
        |base| async move {
            source(&base, "b")
                .fetch_receipt(request(&digest(), 7))
                .await
                .unwrap()
                .unwrap()
        },
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
        |base| async move { source(&base, "b").fetch_receipt(request(&digest(), 7)).await.unwrap() },
        None,
    )
    .await;
}

#[tokio::test]
async fn test_fetch_rejects_a_malformed_reply() {
    http_contract::assert_mapping(
        http_contract::fixed_get(ROUTE, || (StatusCode::OK, "not json").into_response()),
        |base| async move { source(&base, "b").fetch_receipt(request(&digest(), 7)).await },
        Err(TransportError::Malformed),
    )
    .await;
}

#[tokio::test]
async fn test_fetch_rejects_a_reply_whose_digest_is_not_a_sha256() {
    http_contract::assert_mapping(
        http_contract::fixed_get(ROUTE, || {
            Json(ReceiptReply {
                node: "b".to_owned(),
                digest: "not-a-digest".to_owned(),
                size: 7,
            })
            .into_response()
        }),
        |base| async move { source(&base, "b").fetch_receipt(request(&digest(), 7)).await },
        Err(TransportError::Malformed),
    )
    .await;
}

#[tokio::test]
async fn test_fetch_rejects_a_reply_naming_another_node() {
    http_contract::assert_mapping(
        http_contract::fixed_get(ROUTE, || Json(reply("east-1", &digest(), 7)).into_response()),
        |base| async move { source(&base, "east-2").fetch_receipt(request(&digest(), 7)).await },
        Err(TransportError::ReceiptIdentity {
            expected: "east-2".to_owned(),
            actual: "east-1".to_owned(),
        }),
    )
    .await;
}

#[tokio::test]
async fn test_fetch_rejects_a_reply_naming_another_digest() {
    let served = Digest::of(b"other");
    http_contract::assert_mapping(
        http_contract::fixed_get(ROUTE, move || {
            Json(reply("b", &Digest::of(b"other"), 7)).into_response()
        }),
        |base| async move { source(&base, "b").fetch_receipt(request(&digest(), 7)).await },
        Err(TransportError::DigestMismatch {
            expected: digest().as_str().to_owned(),
            actual: served.as_str().to_owned(),
        }),
    )
    .await;
}

#[tokio::test]
async fn test_fetch_rejects_a_reply_reporting_another_size() {
    http_contract::assert_mapping(
        http_contract::fixed_get(ROUTE, || Json(reply("b", &digest(), 9)).into_response()),
        |base| async move { source(&base, "b").fetch_receipt(request(&digest(), 7)).await },
        Err(TransportError::ReceiptSize { expected: 7, actual: 9 }),
    )
    .await;
}

#[test]
fn test_router_rejects_an_empty_token() {
    let dir = tempfile::tempdir().unwrap();
    let error = receipt_router("", "b", BlobStorage::filesystem(dir.path().join("blobs"))).unwrap_err();

    assert!(matches!(error, HttpReceiptError::EmptyToken));
}

#[test]
fn test_router_rejects_an_empty_node() {
    let dir = tempfile::tempdir().unwrap();
    let error = receipt_router(TOKEN, "", BlobStorage::filesystem(dir.path().join("blobs"))).unwrap_err();

    assert!(matches!(error, HttpReceiptError::EmptyNode));
}

#[tokio::test]
async fn test_endpoint_serves_a_held_receipt_end_to_end() {
    let (_dir, blobs, digest) = store_with(BYTES).await;
    let server = TestServer::start(receipt_router(TOKEN, "b", blobs).unwrap()).await;

    let receipt = source(&server.url, "b")
        .fetch_receipt(request(&digest, BYTES.len() as u64))
        .await
        .unwrap()
        .unwrap();

    assert_eq!(receipt.node, "b");
    assert_eq!(receipt.digest, digest);
    assert_eq!(receipt.size, BYTES.len() as u64);
}

#[tokio::test]
async fn test_a_second_source_aimed_at_one_server_gets_no_receipt() {
    let (_dir, blobs, digest) = store_with(BYTES).await;
    let server = TestServer::start(receipt_router(TOKEN, "east-1", blobs).unwrap()).await;

    let error = source(&server.url, "east-2")
        .fetch_receipt(request(&digest, BYTES.len() as u64))
        .await
        .unwrap_err();

    assert_eq!(
        error,
        TransportError::ReceiptIdentity {
            expected: "east-2".to_owned(),
            actual: "east-1".to_owned(),
        }
    );
    assert!(!error.is_retryable());
}

async fn quorum_over(servers: &[&TestServer], bound: &[&str], digest: &Digest) -> (Deadline, usize) {
    let sources: Vec<Arc<dyn ReceiptSource + Send + Sync>> = bound
        .iter()
        .zip(servers)
        .map(|(node, server)| Arc::new(source(&server.url, node)) as Arc<dyn ReceiptSource + Send + Sync>)
        .collect();
    let members = ["writer", "east-1", "east-2"].map(str::to_owned).into();
    let mut ack = FilesystemAck::new(digest.clone(), members, DurabilityPolicy::Everywhere);
    ack.record(ReceiptAck {
        node: "writer".to_owned(),
        digest: digest.clone(),
    });

    let deadline = gather_receipts(
        &sources,
        request(digest, BYTES.len() as u64),
        &mut ack,
        Duration::from_millis(50),
        Duration::from_millis(5),
    )
    .await;
    (deadline, ack.independent_receipts())
}

#[tokio::test]
async fn test_two_sources_aimed_at_one_server_yield_one_receipt() {
    let (_dir, blobs, digest) = store_with(BYTES).await;
    let server = TestServer::start(receipt_router(TOKEN, "east-1", blobs).unwrap()).await;

    let (deadline, receipts) = quorum_over(&[&server, &server], &["east-1", "east-2"], &digest).await;

    assert_eq!((deadline, receipts), (Deadline::Expired, 2));
}

#[tokio::test]
async fn test_distinct_processes_holding_the_bytes_reach_quorum() {
    let (_east_1, east_1_blobs, digest) = store_with(BYTES).await;
    let (_east_2, east_2_blobs, _) = store_with(BYTES).await;
    let east_1 = TestServer::start(receipt_router(TOKEN, "east-1", east_1_blobs).unwrap()).await;
    let east_2 = TestServer::start(receipt_router(TOKEN, "east-2", east_2_blobs).unwrap()).await;

    let (deadline, receipts) = quorum_over(&[&east_1, &east_2], &["east-1", "east-2"], &digest).await;

    assert_eq!((deadline, receipts), (Deadline::Live, 3));
}

#[tokio::test]
async fn test_a_replaced_node_contributes_nothing_under_its_predecessor() {
    let (_east_1, east_1_blobs, digest) = store_with(BYTES).await;
    let (_east_3, east_3_blobs, _) = store_with(BYTES).await;
    let east_1 = TestServer::start(receipt_router(TOKEN, "east-1", east_1_blobs).unwrap()).await;
    let replacement = TestServer::start(receipt_router(TOKEN, "east-3", east_3_blobs).unwrap()).await;

    let (deadline, receipts) = quorum_over(&[&east_1, &replacement], &["east-1", "east-2"], &digest).await;

    assert_eq!((deadline, receipts), (Deadline::Expired, 2));
}

#[tokio::test]
async fn test_endpoint_reports_a_blob_it_does_not_hold_as_absent() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
    let server = TestServer::start(receipt_router(TOKEN, "b", blobs).unwrap()).await;

    let receipt = source(&server.url, "b")
        .fetch_receipt(request(&digest(), 7))
        .await
        .unwrap();

    assert_eq!(receipt, None);
}

#[tokio::test]
async fn test_endpoint_rejects_a_bad_credential() {
    let (_dir, blobs, digest) = store_with(BYTES).await;
    let server = TestServer::start(receipt_router(TOKEN, "b", blobs).unwrap()).await;
    let wrong = HttpReceiptSource::new(&server.url, "b", "wrong", Duration::from_secs(5)).unwrap();

    let error = wrong
        .fetch_receipt(request(&digest, BYTES.len() as u64))
        .await
        .unwrap_err();

    assert_eq!(error, TransportError::Unauthenticated);
}

#[tokio::test]
async fn test_endpoint_rejects_an_unparseable_digest() {
    let dir = tempfile::tempdir().unwrap();
    let router = receipt_router(TOKEN, "b", BlobStorage::filesystem(dir.path().join("blobs"))).unwrap();

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
    let router = receipt_router(TOKEN, "b", blobs).unwrap();
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
