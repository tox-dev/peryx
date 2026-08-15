//! Deterministic single-step replication pulls with caller-managed retry delays.
//!
//! [`advance_once`] validates each batch before buffering any change. Retry outcomes return a delay
//! instead of sleeping, so the caller controls time.

use std::num::NonZeroUsize;
use std::time::Duration;

use crate::backoff::{ReconnectPolicy, Retry};
use crate::channel::{BoundedChannel, buffer_batch};
use crate::peer::{BatchRequest, PeerTransport, TransportError, validate_contiguous};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    /// The step buffered the full batch through `reached`.
    Progressed {
        buffered: usize,
        reached: u64,
        caught_up: bool,
    },
    /// The channel filled after buffering through `reached`; resume there after freeing capacity.
    BackPressured { buffered: usize, reached: u64 },
    /// Retry from the same cursor after `delay`.
    RetryAfter { delay: Duration },
    /// A terminal failure or exhausted retry budget with a stable machine-readable `reason`.
    GaveUp { reason: &'static str },
}

/// Pull and validate one batch after `from`, then buffer as much as `channel` can hold.
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
    // Validate before buffering so a gap cannot advance the frontier and a stalled batch cannot report
    // progress.
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

fn step_from_error(error: &TransportError, policy: &ReconnectPolicy, attempt: u32) -> StepOutcome {
    match policy.on_error(error, attempt) {
        Retry::After(delay) => StepOutcome::RetryAfter { delay },
        Retry::GiveUp { reason } => StepOutcome::GaveUp { reason },
    }
}
