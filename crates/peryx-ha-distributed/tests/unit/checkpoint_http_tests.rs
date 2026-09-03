//! The checkpoint endpoints and the transport that reads them, over a real socket.

use std::num::NonZeroUsize;
use std::time::Duration;

use axum::http::StatusCode;
use peryx_storage::blob::BlobStorage;
use peryx_storage::meta::{CheckpointIdentity, MetaStore};

use crate::peer::{BatchRequest, PeerTransport, TransferLimits};
use crate::protocol::PROTOCOL_VERSION;
use crate::support::TestServer;
use crate::{DEFAULT_MAX_CONCURRENT_BLOB_STREAMS, HttpPeerTransport, TransportError, primary_router_with_limits};
use peryx_driver::BlockingScanExecutor;

const TOKEN: &str = "replication-secret";
const ONE: NonZeroUsize = NonZeroUsize::new(1).expect("1 is non-zero");

fn identity() -> CheckpointIdentity {
    CheckpointIdentity {
        source: "primary-a".to_owned(),
        protocol_version: PROTOCOL_VERSION,
        schema_version: 1,
    }
}

fn rows(meta: &MetaStore, count: usize) {
    for index in 0..count {
        meta.commit_driver_txn(|txn| {
            txn.put(&format!("pypi\u{0}p\u{0}hosted/pkg{index:04}"), b"display")
                .map(|()| ((), vec![b"{}".to_vec()]))
        })
        .unwrap();
    }
}

async fn served(dir: &tempfile::TempDir, meta: MetaStore) -> (TestServer, HttpPeerTransport) {
    let router = primary_router_with_limits(
        "primary-a",
        TOKEN,
        meta,
        BlobStorage::filesystem(dir.path().join("blobs")),
        DEFAULT_MAX_CONCURRENT_BLOB_STREAMS,
        BlockingScanExecutor::new(2),
    )
    .unwrap();
    let server = TestServer::start(router).await;
    let peer = HttpPeerTransport::new(&server.url, TOKEN, TransferLimits::default(), Duration::from_secs(5)).unwrap();
    (server, peer)
}

fn store(dir: &tempfile::TempDir, name: &str) -> MetaStore {
    MetaStore::open(dir.path().join(name)).unwrap()
}

#[tokio::test]
async fn test_the_endpoints_carry_a_whole_checkpoint_to_the_transport_that_reads_them() {
    let dir = tempfile::tempdir().unwrap();
    let writer = store(&dir, "writer.redb");
    rows(&writer, 30);
    let manifest = writer.publish_checkpoint(identity()).unwrap();
    let (_server, peer) = served(&dir, writer.clone()).await;

    assert_eq!(peer.checkpoint_manifest().await.unwrap(), manifest);
    let mut cursor = "r".to_owned();
    let mut received = Vec::new();
    while cursor != "done" {
        let window = peer.checkpoint_chunk(&cursor).await.unwrap();
        received.extend_from_slice(&window.bytes);
        cursor = window.next;
    }

    assert_eq!(received.len() as u64, manifest.bytes);
}

#[tokio::test]
async fn test_a_source_publishing_no_checkpoint_answers_that_it_has_none() {
    let dir = tempfile::tempdir().unwrap();
    let writer = store(&dir, "writer.redb");
    rows(&writer, 2);
    let (_server, peer) = served(&dir, writer.clone()).await;

    assert_eq!(
        peer.checkpoint_manifest().await.unwrap_err(),
        TransportError::CheckpointUnavailable
    );
}

#[tokio::test]
async fn test_a_cursor_the_writer_cannot_read_is_a_client_error() {
    let dir = tempfile::tempdir().unwrap();
    let writer = store(&dir, "writer.redb");
    rows(&writer, 2);
    writer.publish_checkpoint(identity()).unwrap();
    let (server, _peer) = served(&dir, writer.clone()).await;

    let response = reqwest::Client::new()
        .get(format!("{}+replication/v1/checkpoint/chunk?cursor=zzz", server.url))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_the_checkpoint_endpoints_refuse_an_unauthenticated_reader() {
    let dir = tempfile::tempdir().unwrap();
    let writer = store(&dir, "writer.redb");
    rows(&writer, 2);
    writer.publish_checkpoint(identity()).unwrap();
    let (server, _peer) = served(&dir, writer.clone()).await;

    let client = reqwest::Client::new();
    for path in ["+replication/v1/checkpoint", "+replication/v1/checkpoint/chunk"] {
        let response = client.get(format!("{}{path}", server.url)).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{path}");
    }
}

/// A node that installed a checkpoint holds no records below its serial, so a reader asking for them is
/// told to install one rather than given a page that cannot carry it forward.
#[tokio::test]
async fn test_a_reader_below_the_floor_is_told_to_install_a_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let writer = store(&dir, "writer.redb");
    rows(&writer, 4);
    let manifest = writer.publish_checkpoint(identity()).unwrap();
    let installed = store(&dir, "installed.redb");
    installed.begin_checkpoint_transfer(&manifest).unwrap();
    let whole = writer
        .checkpoint_chunk(&peryx_storage::meta::CheckpointCursor::start(), 1 << 20)
        .unwrap();
    installed
        .stage_checkpoint_chunk(&manifest, 0, &whole.bytes, "done")
        .unwrap()
        .unwrap();
    installed
        .install_staged_checkpoint("replication\u{0}state", br#"{"source":"primary-a","serial":4}"#)
        .unwrap();
    let (_server, peer) = served(&dir, installed).await;

    let refused = peer
        .fetch_batch(BatchRequest {
            after: 0,
            max_operations: ONE,
        })
        .await
        .unwrap_err();

    assert_eq!(refused, TransportError::CheckpointRequired);
    assert_eq!(refused.terminal_reason(), Some("checkpoint_required"));
    assert!(!refused.is_retryable());
}

/// The manifest is what says whether a checkpoint exists. The window endpoint answers a writer with
/// nothing published the same way it answers one that has run past the end, so a reader that arrived
/// without a manifest reads an empty transfer rather than a different kind of failure.
#[tokio::test]
async fn test_a_window_from_a_writer_with_nothing_published_is_an_empty_transfer() {
    let dir = tempfile::tempdir().unwrap();
    let writer = store(&dir, "writer.redb");
    rows(&writer, 2);
    let (_server, peer) = served(&dir, writer.clone()).await;

    let window = peer.checkpoint_chunk("r").await.unwrap();

    assert_eq!((window.bytes, window.next), (Vec::new(), "done".to_owned()));
}

/// A store that has published a checkpoint and whose next read fails, so both endpoints answer the
/// failure rather than an absence.
fn failing_after_publish() -> MetaStore {
    let (pages, fault) = peryx_test_support::fault::backend();
    let meta = MetaStore::open_backend(peryx_test_support::fault::faulted(&pages, &fault)).unwrap();
    rows(&meta, 2);
    meta.publish_checkpoint(identity()).unwrap();
    fault.arm(0);
    meta
}

#[tokio::test]
async fn test_a_store_that_cannot_be_read_fails_both_checkpoint_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    let (server, _peer) = served(&dir, failing_after_publish()).await;

    let client = reqwest::Client::new();
    for path in ["+replication/v1/checkpoint", "+replication/v1/checkpoint/chunk"] {
        let response = client
            .get(format!("{}{path}", server.url))
            .bearer_auth(TOKEN)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR, "{path}");
    }
}
