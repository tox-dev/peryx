//! The two bounds the server edge puts on a request body, so a client cannot hold its handler, and
//! whatever that handler locked, for as long as it likes.
//!
//! They catch different failures and neither subsumes the other. A body that stops arriving trips the
//! stall bound, which measures silence. A body that keeps arriving without getting anywhere trips the
//! throughput floor, which measures progress: one frame every twenty-nine seconds satisfies the stall
//! bound forever, and nothing but a budget against delivered bytes ends it.

use std::future::Future as _;
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use http_body::{Body, Frame, SizeHint};
use peryx_core::ThroughputBudget;
use pin_project_lite::pin_project;
use tokio::time::{Instant, Sleep};

pin_project! {
    /// A body that fails once it has gone `stall` without delivering a frame, or once it has taken
    /// longer in total than its delivered bytes have earned against `budget`. The silence wait starts
    /// again at every frame; the budget runs across the whole body.
    pub struct BoundedBody<B> {
        stall: Duration,
        budget: ThroughputBudget,
        started: Option<Instant>,
        #[pin]
        idle: Option<Sleep>,
        #[pin]
        body: B,
    }
}

impl<B> BoundedBody<B> {
    pub const fn new(stall: Duration, budget: ThroughputBudget, body: B) -> Self {
        Self {
            stall,
            budget,
            started: None,
            idle: None,
            body,
        }
    }
}

/// The error a body that stopped arriving ends with, worded for the client that stopped sending rather
/// than for the handler that was reading.
#[derive(Debug)]
pub struct Stalled(Duration);

impl std::error::Error for Stalled {}

impl std::fmt::Display for Stalled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "the request body sent nothing for {:?}", self.0)
    }
}

/// The error a body that kept arriving without getting anywhere ends with.
#[derive(Debug)]
pub struct TooSlow(u64);

impl std::error::Error for TooSlow {}

impl std::fmt::Display for TooSlow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "the request body delivered {} bytes below the sustained throughput floor",
            self.0
        )
    }
}

impl<B> Body for BoundedBody<B>
where
    B: Body,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    type Data = B::Data;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let mut this = self.project();
        let now = Instant::now();
        let started = *this.started.get_or_insert(now);
        if this.idle.is_none() {
            this.idle.set(Some(tokio::time::sleep(*this.stall)));
        }
        let idle = this.idle.as_mut().as_pin_mut().expect("the wait was just armed");
        if idle.poll(cx).is_ready() {
            return Poll::Ready(Some(Err(Box::new(Stalled(*this.stall)))));
        }
        let frame = ready!(this.body.poll_frame(cx));
        this.idle.set(None);
        let Some(frame) = frame else {
            return Poll::Ready(None);
        };
        let frame = match frame {
            Ok(frame) => frame,
            Err(error) => return Poll::Ready(Some(Err(error.into()))),
        };
        // Only data carries bytes, so a trailer earns the body nothing and cannot buy it more time.
        this.budget.deliver(frame.data_ref().map_or(0, bytes::Buf::remaining));
        if this.budget.is_starved(now.saturating_duration_since(started)) {
            return Poll::Ready(Some(Err(Box::new(TooSlow(this.budget.delivered())))));
        }
        Poll::Ready(Some(Ok(frame)))
    }

    /// The length the inner body declares travels through unchanged. A chunked upload's `Content-Range`
    /// is checked against it, so a wrapper that dropped it would turn every ranged chunk into a
    /// rejected one.
    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}
