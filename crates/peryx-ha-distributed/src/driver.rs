//! A deterministic single-step pull driver that composes the replication transport, the bounded
//! change channel, and the reconnect policy into one advance.
//!
//! [`advance_once`] pulls one batch from a [`PeerTransport`], buffers it into a [`BoundedChannel`]
//! with back-pressure, and on a retryable [`TransportError`](crate::peer::TransportError) consults a [`ReconnectPolicy`] to decide
//! whether to retry and how long to wait. It keeps no clock and sleeps for nothing: a retry outcome
//! carries the delay the caller waits before the next step, so the whole decision stays a function of
//! the transport result, the channel state, and the attempt count.
//!
//! [`ReconnectPolicy`]: crate::backoff::ReconnectPolicy

use std::num::NonZeroUsize;
use std::time::Duration;

use crate::backoff::{ReconnectPolicy, Retry};
use crate::channel::{BoundedChannel, buffer_batch};
use crate::peer::{BatchRequest, PeerTransport, TransportError, validate_contiguous};

/// What one [`advance_once`] step produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    /// The batch buffered in full. `buffered` changes joined the channel, `reached` is the highest
    /// serial now queued, and `caught_up` marks that the queue reached the peer's advertised frontier.
    Progressed {
        buffered: usize,
        reached: u64,
        caught_up: bool,
    },
    /// The channel filled before the batch drained. `buffered` changes joined the channel up to
    /// `reached`; the caller applies and pops to free slots, then resumes from `reached`.
    BackPressured { buffered: usize, reached: u64 },
    /// A retryable transport loss. The caller waits `delay`, then retries the step from the same
    /// cursor.
    RetryAfter { delay: Duration },
    /// A terminal failure or an exhausted retry budget. `reason` is the stable machine token for why
    /// the pull stopped.
    GaveUp { reason: &'static str },
}

/// Pull one batch after serial `from`, buffer it, and classify the step.
///
/// On a successful fetch the batch buffers into `channel` until it fills: a full drain reports
/// [`StepOutcome::Progressed`] and a partial one [`StepOutcome::BackPressured`], both carrying the
/// serial the queued changes reach. On a retryable [`TransportError`](crate::peer::TransportError) the `policy` decides, at this
/// `attempt` count, between [`StepOutcome::RetryAfter`] and [`StepOutcome::GaveUp`]; a terminal error
/// gives up at once with its own reason.
pub async fn advance_once<T: PeerTransport>(
    transport: &T,
    channel: &mut BoundedChannel,
    policy: &ReconnectPolicy,
    from: u64,
    request_size: NonZeroUsize,
    attempt: u32,
) -> StepOutcome {
    let frame = match transport
        .fetch_batch(BatchRequest {
            after: from,
            max_operations: request_size,
        })
        .await
    {
        Ok(frame) => frame,
        Err(error) => return step_from_error(&error, policy, attempt),
    };
    let page = frame.page();
    // The frame is validated for contiguity before a single change is buffered, through the same check
    // `drain_to_frontier` runs. A gap or a stalled batch is an error, never a silent frontier skip or a
    // zero-progress success that a `while !caught_up` caller would spin on.
    let (reached, caught_up) = match validate_contiguous(from, page) {
        Ok(progress) => progress,
        Err(error) => return step_from_error(&error, policy, attempt),
    };
    let outcome = buffer_batch(channel, &page.changes);
    if outcome.back_pressure {
        let buffered_reach = page.changes[..outcome.accepted]
            .last()
            .map_or(from, |change| change.serial);
        return StepOutcome::BackPressured {
            buffered: outcome.accepted,
            reached: buffered_reach,
        };
    }
    StepOutcome::Progressed {
        buffered: outcome.accepted,
        reached,
        caught_up,
    }
}

/// Turn a [`TransportError`] into the step it produces: the reconnect policy decides between a timed
/// retry and giving up, so a transport loss and a contiguity failure travel the same decision path.
fn step_from_error(error: &TransportError, policy: &ReconnectPolicy, attempt: u32) -> StepOutcome {
    match policy.on_error(error, attempt) {
        Retry::After(delay) => StepOutcome::RetryAfter { delay },
        Retry::GiveUp { reason } => StepOutcome::GaveUp { reason },
    }
}
