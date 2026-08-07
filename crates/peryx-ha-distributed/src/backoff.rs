//! A deterministic reconnect policy: how long a scheduler waits before retrying a peer after
//! transient transport loss, and when it stops retrying and fails closed.
//!
//! The policy holds no clock and performs no sleep. It maps a [`TransportError`] and the count of
//! consecutive failures to a [`Retry`] decision, so the driver owns the timer and a caller's tests
//! stay deterministic. A terminal error fails closed at once through the error's own
//! [`terminal_reason`], reusing that taxonomy rather than restating it; a retryable one backs off with
//! a bounded exponential delay until it reaches the attempt ceiling, then gives up.
//!
//! [`terminal_reason`]: TransportError::terminal_reason

use std::num::NonZeroU32;
use std::time::Duration;

use crate::peer::TransportError;

/// What a scheduler does after a failed batch fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retry {
    /// Wait this long, then reconnect and retry the drain from the persisted frontier.
    After(Duration),
    /// Stop retrying and fail closed. `reason` is a stable machine token for the terminal cause.
    GiveUp { reason: &'static str },
}

/// The machine reason a retryable failure carries once it exhausts the attempt ceiling. Terminal
/// failures carry their own reason from [`TransportError::terminal_reason`] instead.
pub const RETRY_EXHAUSTED: &str = "retry_exhausted";

/// A bounded exponential reconnect schedule.
///
/// `base` is the first delay; each further attempt multiplies the previous delay by `multiplier`
/// until it reaches `max_delay`, the cap it never exceeds. After `max_attempts` retryable failures the
/// policy gives up rather than retry forever, so a peer that never recovers fails closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectPolicy {
    base: Duration,
    multiplier: NonZeroU32,
    max_delay: Duration,
    max_attempts: NonZeroU32,
}

/// A conservative default: 100 ms doubling to a 30 s cap, giving up after 10 retryable failures.
pub const DEFAULT_RECONNECT_POLICY: ReconnectPolicy = ReconnectPolicy {
    base: Duration::from_millis(100),
    multiplier: NonZeroU32::new(2).expect("2 is non-zero"),
    max_delay: Duration::from_secs(30),
    max_attempts: NonZeroU32::new(10).expect("10 is non-zero"),
};

impl Default for ReconnectPolicy {
    fn default() -> Self {
        DEFAULT_RECONNECT_POLICY
    }
}

impl ReconnectPolicy {
    /// Build a policy from its bounds. `base` is the first delay and `max_delay` the cap; a `base`
    /// above the cap yields the cap.
    #[must_use]
    pub const fn new(base: Duration, multiplier: NonZeroU32, max_delay: Duration, max_attempts: NonZeroU32) -> Self {
        Self {
            base,
            multiplier,
            max_delay,
            max_attempts,
        }
    }

    /// Decide what to do after the `attempt`-th consecutive failed fetch, counting from 1.
    ///
    /// A terminal error fails closed at once, carrying the error's own terminal reason. A retryable
    /// error retries after [`delay_for`](Self::delay_for) until `attempt` reaches `max_attempts`, then
    /// gives up with [`RETRY_EXHAUSTED`].
    #[must_use]
    pub fn on_error(&self, error: &TransportError, attempt: u32) -> Retry {
        if let Some(reason) = error.terminal_reason() {
            return Retry::GiveUp { reason };
        }
        if attempt >= self.max_attempts.get() {
            return Retry::GiveUp {
                reason: RETRY_EXHAUSTED,
            };
        }
        Retry::After(self.delay_for(attempt))
    }

    /// The backoff delay before the `attempt`-th retry: `base * multiplier^(attempt - 1)`, capped at
    /// `max_delay`. Overflow saturates to the cap, so the delay never wraps.
    #[must_use]
    pub fn delay_for(&self, attempt: u32) -> Duration {
        let mut delay = self.base;
        for _ in 1..attempt {
            match delay.checked_mul(self.multiplier.get()) {
                Some(next) if next < self.max_delay => delay = next,
                _ => return self.max_delay,
            }
        }
        delay.min(self.max_delay)
    }
}
