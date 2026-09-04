//! The bound the server edge puts on a request body, so a client that stops sending cannot hold its
//! handler, and whatever that handler locked, until the connection itself dies.

use std::future::Future as _;
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use http_body::{Body, Frame, SizeHint};
use pin_project_lite::pin_project;
use tokio::time::Sleep;

pin_project! {
    /// A body that fails once it has gone `stall` without delivering a frame, with the wait starting
    /// again at every frame. A transfer that keeps arriving is never cut, however long it runs.
    pub struct StallBounded<B> {
        stall: Duration,
        #[pin]
        idle: Option<Sleep>,
        #[pin]
        body: B,
    }
}

impl<B> StallBounded<B> {
    pub const fn new(stall: Duration, body: B) -> Self {
        Self {
            stall,
            idle: None,
            body,
        }
    }
}

/// The error a stalled body ends with, worded for the client that stopped sending rather than for the
/// handler that was reading.
#[derive(Debug)]
pub struct Stalled(Duration);

impl std::error::Error for Stalled {}

impl std::fmt::Display for Stalled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "the request body sent nothing for {:?}", self.0)
    }
}

impl<B> Body for StallBounded<B>
where
    B: Body,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    type Data = B::Data;
    type Error = Box<dyn std::error::Error + Send + Sync>;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let mut this = self.project();
        if this.idle.is_none() {
            this.idle.set(Some(tokio::time::sleep(*this.stall)));
        }
        let idle = this.idle.as_mut().as_pin_mut().expect("the wait was just armed");
        if idle.poll(cx).is_ready() {
            return Poll::Ready(Some(Err(Box::new(Stalled(*this.stall)))));
        }
        let frame = ready!(this.body.poll_frame(cx));
        this.idle.set(None);
        Poll::Ready(frame.map(|frame| frame.map_err(Into::into)))
    }

    /// The length the inner body declares travels through unchanged. A chunked upload's `Content-Range`
    /// is checked against it, so a wrapper that dropped it would turn every ranged chunk into a
    /// rejected one.
    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}
