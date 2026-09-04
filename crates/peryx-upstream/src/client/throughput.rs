//! Bounds a streaming transfer by the work it has actually done.
//!
//! A stream earns time in proportion to the bytes it delivers, so a transfer holding at or above the
//! floor never trips, and one trickling below it runs out of earned time. Expressing the floor as a
//! budget rather than a sampled rate absorbs bursty delivery, which a server that pauses to seek and
//! then sends a batch does legitimately, without letting an indefinite trickle accumulate.

use std::num::NonZeroU64;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use futures_util::Stream;
use peryx_core::ThroughputBudget;

use super::error::UpstreamError;

/// The rate a streaming transfer must hold on average across its whole life.
///
/// Low on purpose: this bounds a transfer rather than enforcing a service level, and a mirror pulling
/// from a congested upstream should finish rather than be cut off for being slow. At this floor a
/// 100 MiB wheel may take 27 minutes and a 2 GiB layer just over 9 hours before the bound fires.
pub const THROUGHPUT_FLOOR: NonZeroU64 = NonZeroU64::new(64 * 1024).unwrap();

/// Time a transfer holds before its delivered bytes have earned it any, covering connection setup and
/// the first response. One idle gap is already allowed by the read timeout, so allowing the same span
/// here keeps a transfer that starts slowly from failing for a stall the client tolerates.
pub const THROUGHPUT_GRACE: Duration = super::READ_IDLE_TIMEOUT;

/// Wraps `body` so a transfer that never idles but never progresses still ends.
pub const fn bounded<Body>(body: Body) -> Bounded<Body> {
    Bounded {
        body,
        budget: ThroughputBudget::new(THROUGHPUT_FLOOR, THROUGHPUT_GRACE),
        started: None,
        stopped: false,
    }
}

pub struct Bounded<Body> {
    body: Body,
    budget: ThroughputBudget,
    started: Option<tokio::time::Instant>,
    stopped: bool,
}

impl<Body> Bounded<Body> {
    /// Whether the transfer has now taken longer than its delivered bytes have earned.
    ///
    /// Checking on arrival rather than on a timer is enough to bound the stream: bytes either keep
    /// coming, and each one is checked here, or they stop and the read timeout ends the connection.
    fn is_starved(&mut self, now: tokio::time::Instant) -> bool {
        let started = *self.started.get_or_insert(now);
        self.budget.is_starved(now.saturating_duration_since(started))
    }
}

impl<Body> Stream for Bounded<Body>
where
    Body: Stream<Item = Result<Bytes, UpstreamError>> + Unpin,
{
    type Item = Result<Bytes, UpstreamError>;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.stopped {
            return Poll::Ready(None);
        }
        let now = tokio::time::Instant::now();
        this.started.get_or_insert(now);
        let chunk = match Pin::new(&mut this.body).poll_next(context) {
            Poll::Ready(Some(Ok(chunk))) => chunk,
            other => return other,
        };
        this.budget.deliver(chunk.len());
        if this.is_starved(now) {
            // The transfer is over, so the error is the last item: a caller that kept polling would
            // otherwise see the failure again rather than the end of the body.
            this.stopped = true;
            return Poll::Ready(Some(Err(UpstreamError::BelowThroughputFloor {
                delivered: this.budget.delivered(),
            })));
        }
        Poll::Ready(Some(Ok(chunk)))
    }
}
