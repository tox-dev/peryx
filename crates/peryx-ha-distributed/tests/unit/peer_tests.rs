use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axum::routing::get;
use axum::{Json, Router};

use crate::HttpPeerTransport;
use crate::peer::{
    BatchFrame, BatchRequest, DEFAULT_TRANSFER_LIMITS, LoopbackPeer, LoopbackTransport, PeerFault, PeerTransport,
    TransferLimits, TransportError, drain_to_frontier,
};
use crate::protocol::{Change, ChangePage, PROTOCOL_VERSION};
use crate::support::TestServer;

fn ops(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).unwrap()
}

fn seeded_peer(source: &str, token: &str, entries: usize) -> LoopbackPeer {
    let mut peer = LoopbackPeer::new(source, token, DEFAULT_TRANSFER_LIMITS);
    for _ in 0..entries {
        peer.append(b"event".to_vec());
    }
    peer
}

fn page(source: &str, after: u64, current_serial: u64, serials: &[u64]) -> ChangePage {
    ChangePage {
        version: PROTOCOL_VERSION,
        source: source.to_owned(),
        after,
        current_serial,
        changes: serials
            .iter()
            .map(|serial| Change {
                serial: *serial,
                event: Vec::new(),
                metadata: Vec::new(),
                blobs: Vec::new(),
            })
            .collect(),
    }
}

struct ScriptedTransport {
    frames: Mutex<std::vec::IntoIter<Result<BatchFrame, TransportError>>>,
    requests: Mutex<Vec<BatchRequest>>,
}

impl ScriptedTransport {
    fn new(frames: Vec<Result<BatchFrame, TransportError>>) -> Self {
        Self {
            frames: Mutex::new(frames.into_iter()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<BatchRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl PeerTransport for ScriptedTransport {
    async fn fetch_batch(&self, request: BatchRequest) -> Result<BatchFrame, TransportError> {
        self.requests.lock().unwrap().push(request);
        self.frames
            .lock()
            .unwrap()
            .next()
            .expect("scripted transport ran out of frames")
    }
}

#[test]
fn test_transfer_limits_default_matches_named_constant() {
    assert_eq!(TransferLimits::default(), DEFAULT_TRANSFER_LIMITS);
}

#[test]
fn test_retryable_transport_loss_is_flagged() {
    assert!(TransportError::Disconnected.is_retryable());
    assert!(TransportError::Timeout.is_retryable());
    assert!(TransportError::ServerError { status: 503 }.is_retryable());
    assert!(TransportError::AtCapacity.is_retryable());
    assert!(!TransportError::Unauthenticated.is_retryable());
    assert!(!TransportError::BadStatus { status: 404 }.is_retryable());
    assert!(!TransportError::Malformed.is_retryable());
    assert!(!TransportError::FrameTooLarge { limit: 1, actual: 2 }.is_retryable());
    assert!(
        !TransportError::DigestMismatch {
            expected: "a".to_owned(),
            actual: "b".to_owned()
        }
        .is_retryable()
    );
    assert!(!TransportError::BlobNotFound { digest: "a".to_owned() }.is_retryable());
}

#[test]
fn test_terminal_reason_is_none_for_retryable_and_named_otherwise() {
    assert_eq!(TransportError::Disconnected.terminal_reason(), None);
    assert_eq!(TransportError::Timeout.terminal_reason(), None);
    assert_eq!(TransportError::ServerError { status: 503 }.terminal_reason(), None);
    assert_eq!(TransportError::AtCapacity.terminal_reason(), None);
    assert_eq!(
        TransportError::Unauthenticated.terminal_reason(),
        Some("unauthenticated")
    );
    assert_eq!(
        TransportError::DigestMismatch {
            expected: "a".to_owned(),
            actual: "b".to_owned()
        }
        .terminal_reason(),
        Some("digest_mismatch")
    );
    assert_eq!(
        TransportError::BlobNotFound { digest: "a".to_owned() }.terminal_reason(),
        Some("blob_not_found")
    );
    assert_eq!(
        TransportError::BadStatus { status: 503 }.terminal_reason(),
        Some("bad_status")
    );
    assert_eq!(TransportError::Malformed.terminal_reason(), Some("malformed"));
    assert_eq!(
        TransportError::FrameTooLarge { limit: 1, actual: 2 }.terminal_reason(),
        Some("frame_too_large")
    );
    assert_eq!(
        TransportError::TooManyOperations { limit: 1, actual: 2 }.terminal_reason(),
        Some("too_many_operations")
    );
    assert_eq!(
        TransportError::SourceChanged {
            expected: "a".to_owned(),
            actual: "b".to_owned()
        }
        .terminal_reason(),
        Some("source_changed")
    );
    assert_eq!(
        TransportError::FrontierGap { expected: 1, actual: 3 }.terminal_reason(),
        Some("frontier_gap")
    );
    assert_eq!(
        TransportError::EmptyBatch { frontier: 5, after: 2 }.terminal_reason(),
        Some("empty_batch")
    );
    assert_eq!(
        TransportError::ReceiptIdentity {
            expected: "a".to_owned(),
            actual: "b".to_owned()
        }
        .terminal_reason(),
        Some("receipt_identity")
    );
    assert_eq!(
        TransportError::ReceiptSize { expected: 1, actual: 2 }.terminal_reason(),
        Some("receipt_size")
    );
}

#[tokio::test]
async fn test_fetch_batch_frames_bounded_changes_and_frontier() {
    let peer = seeded_peer("primary-a", "secret", 3);
    let transport = LoopbackTransport::connect(&peer, "secret");
    let frame = transport
        .fetch_batch(BatchRequest {
            after: 1,
            max_operations: ops(5),
        })
        .await
        .unwrap();
    let serials: Vec<u64> = frame.page().changes.iter().map(|change| change.serial).collect();
    assert_eq!(serials, vec![2, 3]);
    assert_eq!(frame.frontier(), ("primary-a", 3));
    assert_eq!(
        frame.encoded_len(),
        serde_json::to_vec(frame.page()).unwrap().len() as u64
    );
}

#[tokio::test]
async fn test_fetch_batch_caps_operations_per_batch() {
    let peer = seeded_peer("primary-a", "secret", 5);
    let transport = LoopbackTransport::connect(&peer, "secret");
    let frame = transport
        .fetch_batch(BatchRequest {
            after: 0,
            max_operations: ops(2),
        })
        .await
        .unwrap();
    let serials: Vec<u64> = frame.page().changes.iter().map(|change| change.serial).collect();
    assert_eq!(serials, vec![1, 2]);
    assert_eq!(frame.frontier(), ("primary-a", 5));
}

#[tokio::test]
async fn test_fetch_batch_rejects_a_wrong_credential() {
    let peer = seeded_peer("primary-a", "secret", 1);
    let transport = LoopbackTransport::connect(&peer, "guessed");
    let error = transport
        .fetch_batch(BatchRequest {
            after: 0,
            max_operations: ops(1),
        })
        .await
        .unwrap_err();
    assert_eq!(error, TransportError::Unauthenticated);
}

#[tokio::test]
async fn test_fetch_batch_refuses_an_over_limit_request() {
    let peer = LoopbackPeer::new(
        "primary-a",
        "secret",
        TransferLimits {
            max_operations: ops(4),
            ..DEFAULT_TRANSFER_LIMITS
        },
    );
    let transport = LoopbackTransport::connect(&peer, "secret");
    let error = transport
        .fetch_batch(BatchRequest {
            after: 0,
            max_operations: ops(5),
        })
        .await
        .unwrap_err();
    assert_eq!(error, TransportError::TooManyOperations { limit: 4, actual: 5 });
}

#[tokio::test]
async fn test_fetch_batch_refuses_an_oversized_frame() {
    let mut peer = LoopbackPeer::new(
        "primary-a",
        "secret",
        TransferLimits {
            max_operations: ops(8),
            max_encoded_bytes: std::num::NonZeroU64::new(64).unwrap(),
        },
    );
    peer.append(vec![0; 512]);
    let transport = LoopbackTransport::connect(&peer, "secret");
    let error = transport
        .fetch_batch(BatchRequest {
            after: 0,
            max_operations: ops(8),
        })
        .await
        .unwrap_err();
    assert!(
        matches!(error, TransportError::FrameTooLarge { limit: 64, actual } if actual > 64),
        "an oversized frame reports its true byte length"
    );
}

#[tokio::test]
async fn test_injected_disconnect_surfaces_as_retryable() {
    let peer = seeded_peer("primary-a", "secret", 1);
    peer.inject(PeerFault::Disconnect);
    let transport = LoopbackTransport::connect(&peer, "secret");
    let error = transport
        .fetch_batch(BatchRequest {
            after: 0,
            max_operations: ops(1),
        })
        .await
        .unwrap_err();
    assert_eq!(error, TransportError::Disconnected);
    assert!(error.is_retryable());
}

#[tokio::test]
async fn test_injected_timeout_clears_after_one_request() {
    let peer = seeded_peer("primary-a", "secret", 1);
    peer.inject(PeerFault::Timeout);
    let transport = LoopbackTransport::connect(&peer, "secret");
    let first = transport
        .fetch_batch(BatchRequest {
            after: 0,
            max_operations: ops(1),
        })
        .await
        .unwrap_err();
    assert_eq!(first, TransportError::Timeout);
    let recovered = transport
        .fetch_batch(BatchRequest {
            after: 0,
            max_operations: ops(1),
        })
        .await
        .unwrap();
    assert_eq!(recovered.frontier(), ("primary-a", 1));
}

#[tokio::test]
async fn test_drain_collects_every_change_when_caught_up() {
    let peer = seeded_peer("primary-a", "secret", 5);
    let transport = LoopbackTransport::connect(&peer, "secret");
    let sync = drain_to_frontier(&transport, 0, ops(2), ops(100)).await.unwrap();
    assert!(sync.caught_up);
    assert_eq!(sync.through, 5);
    assert_eq!(sync.source, "primary-a");
    let serials: Vec<u64> = sync.changes.iter().map(|change| change.serial).collect();
    assert_eq!(serials, vec![1, 2, 3, 4, 5]);
}

#[tokio::test]
async fn test_drain_resumes_from_a_mid_frontier() {
    let peer = seeded_peer("primary-a", "secret", 4);
    let transport = LoopbackTransport::connect(&peer, "secret");
    let sync = drain_to_frontier(&transport, 2, ops(10), ops(100)).await.unwrap();
    assert!(sync.caught_up);
    let serials: Vec<u64> = sync.changes.iter().map(|change| change.serial).collect();
    assert_eq!(serials, vec![3, 4]);
}

#[tokio::test]
async fn test_drain_limits_the_last_request_to_the_memory_budget() {
    let peer = seeded_peer("primary-a", "secret", 5);
    let transport = LoopbackTransport::connect(&peer, "secret");
    let sync = drain_to_frontier(&transport, 0, ops(2), ops(3)).await.unwrap();
    assert!(!sync.caught_up);
    assert_eq!(sync.through, 3);
    assert_eq!(
        sync.changes.iter().map(|change| change.serial).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    let resumed = drain_to_frontier(&transport, sync.through, ops(2), ops(3))
        .await
        .unwrap();
    assert!(resumed.caught_up);
    let serials: Vec<u64> = resumed.changes.iter().map(|change| change.serial).collect();
    assert_eq!(serials, vec![4, 5]);
}

#[tokio::test]
async fn test_drain_rejects_an_over_count_page_without_a_partial_result() {
    let transport = ScriptedTransport::new(vec![
        Ok(BatchFrame::new(page("primary-a", 0, 4, &[1, 2]))),
        Ok(BatchFrame::new(page("primary-a", 2, 4, &[3, 4]))),
    ]);

    let error = drain_to_frontier(&transport, 0, ops(2), ops(3)).await.unwrap_err();

    assert_eq!(error, TransportError::TooManyOperations { limit: 1, actual: 2 });
    assert_eq!(
        transport.requests(),
        vec![
            BatchRequest {
                after: 0,
                max_operations: ops(2),
            },
            BatchRequest {
                after: 2,
                max_operations: ops(1),
            },
        ]
    );
}

#[tokio::test]
async fn test_drain_of_an_empty_peer_is_immediately_caught_up() {
    let peer = seeded_peer("primary-a", "secret", 0);
    let transport = LoopbackTransport::connect(&peer, "secret");
    let sync = drain_to_frontier(&transport, 0, ops(2), ops(3)).await.unwrap();
    assert!(sync.caught_up);
    assert_eq!(sync.through, 0);
    assert!(sync.changes.is_empty());
}

#[tokio::test]
async fn test_drain_propagates_a_transport_error() {
    let peer = seeded_peer("primary-a", "secret", 3);
    let transport = LoopbackTransport::connect(&peer, "wrong");
    let error = drain_to_frontier(&transport, 0, ops(2), ops(100)).await.unwrap_err();
    assert_eq!(error, TransportError::Unauthenticated);
}

#[tokio::test]
async fn test_drain_rejects_a_moving_source() {
    let transport = ScriptedTransport::new(vec![
        Ok(BatchFrame::new(page("primary-a", 0, 4, &[1, 2]))),
        Ok(BatchFrame::new(page("primary-b", 2, 4, &[3, 4]))),
    ]);
    let error = drain_to_frontier(&transport, 0, ops(2), ops(100)).await.unwrap_err();
    assert_eq!(
        error,
        TransportError::SourceChanged {
            expected: "primary-a".to_owned(),
            actual: "primary-b".to_owned()
        }
    );
}

#[tokio::test]
async fn test_http_drain_rejects_a_moving_source() {
    let calls = Arc::new(AtomicUsize::new(0));
    let handler_calls = Arc::clone(&calls);
    let server = TestServer::start(Router::new().route(
        "/+replication/v1/changes",
        get(move || {
            let call = handler_calls.fetch_add(1, Ordering::Relaxed);
            async move {
                Json(if call == 0 {
                    page("primary-a", 0, 4, &[1, 2])
                } else {
                    page("primary-b", 2, 4, &[3, 4])
                })
            }
        }),
    ))
    .await;
    let transport =
        HttpPeerTransport::new(&server.url, "secret", DEFAULT_TRANSFER_LIMITS, Duration::from_secs(1)).unwrap();

    assert_eq!(
        drain_to_frontier(&transport, 0, ops(2), ops(100)).await,
        Err(TransportError::SourceChanged {
            expected: "primary-a".to_owned(),
            actual: "primary-b".to_owned(),
        })
    );
}

#[tokio::test]
async fn test_drain_rejects_a_batch_that_starts_off_frontier() {
    let transport = ScriptedTransport::new(vec![Ok(BatchFrame::new(page("primary-a", 3, 4, &[4])))]);
    let error = drain_to_frontier(&transport, 0, ops(2), ops(100)).await.unwrap_err();
    assert_eq!(error, TransportError::FrontierGap { expected: 0, actual: 3 });
}

#[tokio::test]
async fn test_drain_rejects_a_non_contiguous_serial() {
    let transport = ScriptedTransport::new(vec![Ok(BatchFrame::new(page("primary-a", 0, 4, &[1, 3])))]);
    let error = drain_to_frontier(&transport, 0, ops(2), ops(100)).await.unwrap_err();
    assert_eq!(error, TransportError::FrontierGap { expected: 2, actual: 3 });
}

#[tokio::test]
async fn test_drain_rejects_an_empty_batch_behind_the_frontier() {
    let transport = ScriptedTransport::new(vec![Ok(BatchFrame::new(page("primary-a", 0, 4, &[])))]);
    let error = drain_to_frontier(&transport, 0, ops(2), ops(100)).await.unwrap_err();
    assert_eq!(error, TransportError::EmptyBatch { frontier: 4, after: 0 });
}
