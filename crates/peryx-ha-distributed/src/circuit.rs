//! Excludes failing blob sources from placement selection after a configured number of consecutive
//! losses. The caller supplies logical time. A source becomes eligible after its cooldown; success
//! closes it and another failure starts a new cooldown.

use std::collections::HashMap;
use std::time::Duration;

pub const DEFAULT_CIRCUIT: CircuitConfig = CircuitConfig {
    trip_after: 3,
    cooldown: Duration::from_secs(30),
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CircuitConfig {
    /// Zero is treated as one.
    pub trip_after: u32,
    pub cooldown: Duration,
}

impl Default for CircuitConfig {
    fn default() -> Self {
        DEFAULT_CIRCUIT
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Closed { failures: u32 },
    Open { until: Duration },
}

#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    config: CircuitConfig,
    sources: HashMap<String, State>,
}

impl CircuitBreaker {
    #[must_use]
    pub fn new(config: CircuitConfig) -> Self {
        Self {
            config,
            sources: HashMap::new(),
        }
    }

    /// Unseen and closed sources are eligible. Open sources become eligible at the cooldown deadline.
    #[must_use]
    pub fn available(&self, source: &str, now: Duration) -> bool {
        match self.sources.get(source) {
            None | Some(State::Closed { .. }) => true,
            Some(State::Open { until }) => now >= *until,
        }
    }

    pub fn record_success(&mut self, source: &str) {
        self.sources.insert(source.to_owned(), State::Closed { failures: 0 });
    }

    /// A failure from an eligible open source starts a new cooldown.
    pub fn record_failure(&mut self, source: &str, now: Duration) {
        let threshold = self.config.trip_after.max(1);
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
