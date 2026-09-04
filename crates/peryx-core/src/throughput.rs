//! What a transfer has earned by the bytes it has moved.
//!
//! A stream earns time in proportion to the bytes it delivers, so a transfer holding at or above the
//! floor never runs out and one trickling below it does. Expressing the floor as a budget rather than a
//! sampled rate absorbs bursty delivery, which a peer that pauses to seek and then sends a batch does
//! legitimately, without letting an indefinite trickle accumulate.
//!
//! The budget keeps no clock. Each direction measures elapsed time with the clock it already holds and
//! asks only whether that much time has been earned, which is the whole of what the two share.

use std::num::NonZeroU64;
use std::time::Duration;

/// The time a transfer has earned, against the time it has taken.
#[derive(Debug)]
pub struct ThroughputBudget {
    floor_bytes_per_second: NonZeroU64,
    grace: Duration,
    delivered: u64,
}

impl ThroughputBudget {
    /// A budget that allows `grace` before any delivery has earned time, covering setup and the first
    /// frame, and one second per `floor_bytes_per_second` delivered after that.
    #[must_use]
    pub const fn new(floor_bytes_per_second: NonZeroU64, grace: Duration) -> Self {
        Self {
            floor_bytes_per_second,
            grace,
            delivered: 0,
        }
    }

    /// Count bytes that have arrived.
    pub fn deliver(&mut self, bytes: usize) {
        self.delivered = self.delivered.saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
    }

    #[must_use]
    pub const fn delivered(&self) -> u64 {
        self.delivered
    }

    /// Whether `elapsed` has outrun what the delivered bytes earned.
    #[must_use]
    pub fn is_starved(&self, elapsed: Duration) -> bool {
        let nanos = u128::from(self.delivered) * 1_000_000_000 / u128::from(self.floor_bytes_per_second.get());
        let earned = self
            .grace
            .saturating_add(Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX)));
        elapsed > earned
    }
}
