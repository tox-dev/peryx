//! Maps transport failures and consecutive attempt counts to retry decisions. The caller owns the
//! clock and timer. Terminal errors fail closed; retryable errors use bounded exponential backoff
//! until the attempt limit.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash as _, Hasher as _};
use std::num::NonZeroU32;
use std::time::Duration;

use crate::peer::TransportError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retry {
    /// Retry from the persisted frontier after this delay.
    After(Duration),
    /// Stop retrying. `reason` is a stable machine token.
    GiveUp { reason: &'static str },
}

/// Retryable failures use this reason after exhausting the attempt limit; terminal failures retain
/// [`TransportError::terminal_reason`].
pub const RETRY_EXHAUSTED: &str = "retry_exhausted";

/// `base` is the first delay. Later attempts multiply it by `multiplier`, capped at `max_delay`.
/// The policy gives up after `max_attempts` retryable failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconnectPolicy {
    base: Duration,
    multiplier: NonZeroU32,
    max_delay: Duration,
    max_attempts: NonZeroU32,
}

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
    /// A `base` above `max_delay` yields `max_delay`.
    #[must_use]
    pub const fn new(base: Duration, multiplier: NonZeroU32, max_delay: Duration, max_attempts: NonZeroU32) -> Self {
        Self {
            base,
            multiplier,
            max_delay,
            max_attempts,
        }
    }

    /// Treats `attempt` as one-based. Terminal errors fail closed on the first attempt. Retryable
    /// errors give up with [`RETRY_EXHAUSTED`] at `max_attempts`.
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

    /// Returns `base * multiplier^(attempt - 1)`, capped at `max_delay`. Overflow also yields the cap.
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

    pub(crate) fn quarantine_delay(&self) -> Duration {
        self.delay_for(self.max_attempts.get())
    }
}

/// Derives retry jitter from source identity and attempt without shared random state, so peers that
/// failed together do not come back together and a test can predict the spread it will see.
#[must_use]
pub fn jitter(source: &str, attempt: u32, window: Duration) -> Duration {
    let span = u64::try_from(window.as_nanos()).unwrap_or(u64::MAX);
    if span == 0 {
        return Duration::ZERO;
    }
    let mut hasher = DefaultHasher::new();
    source.hash(&mut hasher);
    attempt.hash(&mut hasher);
    Duration::from_nanos(hasher.finish() % span)
}
