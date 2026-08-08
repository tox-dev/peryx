//! A per-source circuit breaker for read-through placement selection.
//!
//! A read-through picks among the verified placements of a blob. When one source keeps losing, retrying
//! it burns the request's attempt budget and its latency on a peer that will not answer. The breaker
//! trips a source open after it fails a threshold number of times in a row, holds it open for a
//! cooldown, then lets a single probe through; a probe that succeeds closes the source, one that fails
//! re-opens it. A source that answers resets its failure count.
//!
//! The breaker holds no clock and spawns nothing: every method takes the caller's logical `now`, so a
//! test drives it with explicit timestamps and the whole thing stays deterministic.

use std::collections::HashMap;
use std::time::Duration;

/// The default trip threshold and cooldown: three consecutive losses open a source, and it stays open
/// for thirty seconds before a probe may try it again.
pub const DEFAULT_CIRCUIT: CircuitConfig = CircuitConfig {
    trip_after: 3,
    cooldown: Duration::from_secs(30),
};

/// The bounds a [`CircuitBreaker`] trips on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CircuitConfig {
    /// Consecutive failures that open a source. Zero is treated as one, so a source always trips.
    pub trip_after: u32,
    /// How long a source stays open before a single probe may try it.
    pub cooldown: Duration,
}

impl Default for CircuitConfig {
    fn default() -> Self {
        DEFAULT_CIRCUIT
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Answering: `failures` consecutive losses recorded, still below the trip threshold.
    Closed { failures: u32 },
    /// Tripped until `until`; the first attempt at or after it is a half-open probe.
    Open { until: Duration },
}

/// A set of sources, each with its own trip state, keyed by an opaque source identity such as a data
/// center name.
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    config: CircuitConfig,
    sources: HashMap<String, State>,
}

impl CircuitBreaker {
    /// Build a breaker with the given bounds and no sources yet seen.
    #[must_use]
    pub fn new(config: CircuitConfig) -> Self {
        Self {
            config,
            sources: HashMap::new(),
        }
    }

    /// Whether `source` may be tried at `now`. An unseen or closed source is available; an open source
    /// is available only once its cooldown has elapsed, when the next attempt is a half-open probe.
    #[must_use]
    pub fn available(&self, source: &str, now: Duration) -> bool {
        match self.sources.get(source) {
            None | Some(State::Closed { .. }) => true,
            Some(State::Open { until }) => now >= *until,
        }
    }

    /// Record that `source` answered: it closes and its failure count resets.
    pub fn record_success(&mut self, source: &str) {
        self.sources.insert(source.to_owned(), State::Closed { failures: 0 });
    }

    /// Record that `source` lost at `now`. A closed source counts the loss and trips open once it
    /// reaches the threshold; an open source that a probe just failed re-opens for a fresh cooldown.
    pub fn record_failure(&mut self, source: &str, now: Duration) {
        let threshold = self.config.trip_after.max(1);
        // A failed half-open probe (an open source) re-trips; an unseen source starts from zero.
        let failures = match self.sources.get(source) {
            Some(State::Closed { failures }) => *failures,
            Some(State::Open { .. }) => threshold,
            None => 0,
        };
        let next = if failures + 1 < threshold {
            State::Closed { failures: failures + 1 }
        } else {
            State::Open {
                until: now + self.config.cooldown,
            }
        };
        self.sources.insert(source.to_owned(), next);
    }
}
