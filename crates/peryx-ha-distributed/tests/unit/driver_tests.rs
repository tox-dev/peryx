use std::num::NonZeroUsize;

use async_trait::async_trait;

use crate::backoff::{DEFAULT_RECONNECT_POLICY, RETRY_EXHAUSTED};
use crate::channel::BoundedChannel;
use crate::driver::{StepOutcome, advance_once};
use crate::peer::{
    BatchFrame, BatchRequest, DEFAULT_TRANSFER_LIMITS, LoopbackPeer, LoopbackTransport, PeerFault, PeerTransport,
    TransportError,
};
use crate::protocol::{Change, ChangePage, PROTOCOL_VERSION};

struct FixedFrame(BatchFrame);

#[async_trait]
impl PeerTransport for FixedFrame {
    async fn fetch_batch(&self, _request: BatchRequest) -> Result<BatchFrame, TransportError> {
        Ok(self.0.clone())
    }
}

fn frame(after: u64, current_serial: u64, serials: &[u64]) -> BatchFrame {
    BatchFrame::new(ChangePage {
        version: PROTOCOL_VERSION,
        source: "primary".to_owned(),
        after,
        current_serial,
        changes: serials.iter().map(|serial| change(*serial)).collect(),
    })
}

fn seeded_peer(entries: usize) -> LoopbackPeer {
    let mut peer = LoopbackPeer::new("primary", "secret", DEFAULT_TRANSFER_LIMITS);
    for _ in 0..entries {
        peer.append(b"event".to_vec());
    }
    peer
}

fn channel(capacity: usize) -> BoundedChannel {
    BoundedChannel::new(NonZeroUsize::new(capacity).unwrap())
}

fn size(operations: usize) -> NonZeroUsize {
    NonZeroUsize::new(operations).unwrap()
}

fn change(serial: u64) -> Change {
    Change {
        serial,
        event: serial.to_le_bytes().to_vec(),
        metadata: Vec::new(),
        blobs: Vec::new(),
    }
}

#[tokio::test]
async fn test_progressed_buffers_the_whole_batch_and_marks_catch_up() {
    let peer = seeded_peer(2);
    let transport = LoopbackTransport::connect(&peer, "secret");
    let mut channel = channel(5);

    let outcome = advance_once(&transport, &mut channel, &DEFAULT_RECONNECT_POLICY, 0, size(10), 1).await;

    assert_eq!(
        outcome,
        StepOutcome::Progressed {
            buffered: 2,
            reached: 2,
            caught_up: true,
        }
    );
    assert_eq!(channel.len(), 2);
}

#[tokio::test]
async fn test_progressed_short_of_the_frontier_is_not_caught_up() {
    let peer = seeded_peer(5);
    let transport = LoopbackTransport::connect(&peer, "secret");
    let mut channel = channel(5);

    let outcome = advance_once(&transport, &mut channel, &DEFAULT_RECONNECT_POLICY, 0, size(2), 1).await;

    assert_eq!(
        outcome,
        StepOutcome::Progressed {
            buffered: 2,
            reached: 2,
            caught_up: false,
        }
    );
}

#[tokio::test]
async fn test_back_pressure_stops_at_the_channel_bound() {
    let peer = seeded_peer(5);
    let transport = LoopbackTransport::connect(&peer, "secret");
    let mut channel = channel(1);

    let outcome = advance_once(&transport, &mut channel, &DEFAULT_RECONNECT_POLICY, 0, size(10), 1).await;

    assert_eq!(
        outcome,
        StepOutcome::BackPressured {
            buffered: 1,
            reached: 1
        }
    );
    assert!(channel.is_full());
}

#[tokio::test]
async fn test_back_pressure_on_a_full_channel_buffers_nothing() {
    let peer = seeded_peer(3);
    let transport = LoopbackTransport::connect(&peer, "secret");
    let mut channel = channel(1);
    channel.try_push(change(99)).unwrap();

    let outcome = advance_once(&transport, &mut channel, &DEFAULT_RECONNECT_POLICY, 0, size(10), 1).await;

    assert_eq!(
        outcome,
        StepOutcome::BackPressured {
            buffered: 0,
            reached: 0
        }
    );
}

#[tokio::test]
async fn test_retryable_loss_returns_the_backoff_delay() {
    let peer = seeded_peer(2);
    peer.inject(PeerFault::Disconnect);
    let transport = LoopbackTransport::connect(&peer, "secret");
    let mut channel = channel(5);

    let outcome = advance_once(&transport, &mut channel, &DEFAULT_RECONNECT_POLICY, 0, size(10), 1).await;

    assert_eq!(
        outcome,
        StepOutcome::RetryAfter {
            delay: DEFAULT_RECONNECT_POLICY.delay_for(1),
        }
    );
    assert!(channel.is_empty());
}

#[tokio::test]
async fn test_terminal_error_gives_up_with_its_reason() {
    let peer = seeded_peer(2);
    let transport = LoopbackTransport::connect(&peer, "wrong-token");
    let mut channel = channel(5);

    let outcome = advance_once(&transport, &mut channel, &DEFAULT_RECONNECT_POLICY, 0, size(10), 1).await;

    assert_eq!(
        outcome,
        StepOutcome::GaveUp {
            reason: "unauthenticated"
        }
    );
}

#[tokio::test]
async fn test_exhausted_retry_budget_gives_up() {
    let peer = seeded_peer(2);
    peer.inject(PeerFault::Timeout);
    let transport = LoopbackTransport::connect(&peer, "secret");
    let mut channel = channel(5);

    let outcome = advance_once(&transport, &mut channel, &DEFAULT_RECONNECT_POLICY, 0, size(10), 10).await;

    assert_eq!(
        outcome,
        StepOutcome::GaveUp {
            reason: RETRY_EXHAUSTED
        }
    );
}

#[tokio::test]
async fn test_advance_rejects_a_serial_gap_instead_of_skipping() {
    let transport = FixedFrame(frame(0, 51, &[50, 51]));
    let mut channel = channel(5);

    let outcome = advance_once(&transport, &mut channel, &DEFAULT_RECONNECT_POLICY, 0, size(10), 1).await;

    assert_eq!(outcome, StepOutcome::GaveUp { reason: "frontier_gap" });
    assert!(channel.is_empty());
}

#[tokio::test]
async fn test_advance_rejects_an_empty_batch_behind_the_frontier() {
    let transport = FixedFrame(frame(0, 5, &[]));
    let mut channel = channel(5);

    let outcome = advance_once(&transport, &mut channel, &DEFAULT_RECONNECT_POLICY, 0, size(10), 1).await;

    assert_eq!(outcome, StepOutcome::GaveUp { reason: "empty_batch" });
    assert!(channel.is_empty());
}
