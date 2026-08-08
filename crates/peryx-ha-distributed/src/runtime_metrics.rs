//! Bounded availability metrics for the replica sync loop.
//!
//! The series are process-global on a [read replica](@/core/high-availability.md) and carry no
//! per-object label: `class` on the error counter and `le` on the latency histogram are the only
//! labels, and each draws from a fixed vocabulary. A replicated store therefore adds no series as it
//! grows, so the exposition stays bounded whatever the topology.

use std::fmt::Write as _;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use crate::{SyncError, SyncOutcome};
use peryx_driver::PrometheusSource;

/// Why a replica sync cycle failed, the only label the error counter carries. The set is closed so
/// the counter stays at one series per class rather than one per distinct error string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncErrorClass {
    /// The primary speaks a protocol version this replica cannot apply.
    Schema,
    /// The request to the primary never produced a page: a transport or upstream failure.
    Transport,
    /// A page reached the replica but failed validation or local commit.
    Apply,
}

impl SyncErrorClass {
    const ALL: [Self; 3] = [Self::Schema, Self::Transport, Self::Apply];

    const fn as_str(self) -> &'static str {
        match self {
            Self::Schema => "schema",
            Self::Transport => "transport",
            Self::Apply => "apply",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Schema => 0,
            Self::Transport => 1,
            Self::Apply => 2,
        }
    }

    /// Classify a sync failure into its bounded label. An unsupported version is a `schema` fault a
    /// retry cannot fix; a failed primary request is `transport`; every page-validation or local
    /// commit failure is `apply`.
    const fn of(error: &SyncError) -> Self {
        match error {
            SyncError::UnsupportedVersion { .. } => Self::Schema,
            SyncError::Primary(_) => Self::Transport,
            _ => Self::Apply,
        }
    }
}

/// Upper bounds, in seconds, of the apply-latency histogram. Fixed so the series count never depends
/// on the observed values; the render adds an implicit `+Inf` bucket from the total count.
const LATENCY_BUCKETS_SECONDS: [f64; 6] = [0.005, 0.025, 0.1, 0.5, 2.5, 10.0];

/// Cumulative bucket counts, the observation count, and the summed latency, the state a Prometheus
/// histogram renders from.
#[derive(Debug, Default, Clone, Copy)]
struct LatencyHistogram {
    buckets: [u64; LATENCY_BUCKETS_SECONDS.len()],
    count: u64,
    sum_seconds: f64,
}

impl LatencyHistogram {
    fn observe(&mut self, elapsed: Duration) {
        let seconds = elapsed.as_secs_f64();
        self.count += 1;
        self.sum_seconds += seconds;
        for (bound, bucket) in LATENCY_BUCKETS_SECONDS.iter().zip(&mut self.buckets) {
            if seconds <= *bound {
                *bucket += 1;
            }
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct AvailabilityState {
    cycles: u64,
    errors: [u64; SyncErrorClass::ALL.len()],
    pending_serials: u64,
    latency: LatencyHistogram,
}

/// Bounded counters for replica sync cycles.
#[derive(Debug, Default)]
pub struct AvailabilityMetrics {
    state: Mutex<AvailabilityState>,
}

impl AvailabilityMetrics {
    fn with<R>(&self, edit: impl FnOnce(&mut AvailabilityState) -> R) -> R {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        edit(&mut state)
    }

    /// Record an applied cycle: advance the cycle counter, set the queue depth to the serials the
    /// primary has that the replica has not, and observe the cycle latency.
    pub fn record_cycle(&self, outcome: SyncOutcome, elapsed: Duration) {
        self.with(|state| {
            state.cycles += 1;
            state.pending_serials = outcome.primary_serial.saturating_sub(outcome.serial);
            state.latency.observe(elapsed);
        });
    }

    /// Record a failed cycle under its bounded class, leaving the queue depth unchanged because no
    /// page was applied.
    pub fn record_error(&self, error: &SyncError, elapsed: Duration) {
        let class = SyncErrorClass::of(error);
        self.with(|state| {
            state.cycles += 1;
            state.errors[class.index()] += 1;
            state.latency.observe(elapsed);
        });
    }
}

impl PrometheusSource for AvailabilityMetrics {
    fn write_metrics(&self, body: &mut String) {
        let state = *self.state.lock().unwrap_or_else(PoisonError::into_inner);
        body.push_str(
            "# HELP peryx_availability_sync_cycles_total Replica sync cycles attempted.\n\
             # TYPE peryx_availability_sync_cycles_total counter\n",
        );
        let _ = writeln!(body, "peryx_availability_sync_cycles_total {}", state.cycles);
        body.push_str(
            "# HELP peryx_availability_sync_errors_total Replica sync cycles that failed, by class.\n\
             # TYPE peryx_availability_sync_errors_total counter\n",
        );
        for class in SyncErrorClass::ALL {
            let _ = writeln!(
                body,
                "peryx_availability_sync_errors_total{{class=\"{}\"}} {}",
                class.as_str(),
                state.errors[class.index()]
            );
        }
        body.push_str(
            "# HELP peryx_availability_pending_serials Serials the primary has that the replica has not applied.\n\
             # TYPE peryx_availability_pending_serials gauge\n",
        );
        let _ = writeln!(body, "peryx_availability_pending_serials {}", state.pending_serials);
        body.push_str(
            "# HELP peryx_availability_apply_seconds Replica apply-cycle latency in seconds.\n\
             # TYPE peryx_availability_apply_seconds histogram\n",
        );
        for (bound, count) in LATENCY_BUCKETS_SECONDS.iter().zip(state.latency.buckets) {
            let _ = writeln!(
                body,
                "peryx_availability_apply_seconds_bucket{{le=\"{bound}\"}} {count}"
            );
        }
        let _ = writeln!(
            body,
            "peryx_availability_apply_seconds_bucket{{le=\"+Inf\"}} {count}\n\
             peryx_availability_apply_seconds_sum {sum}\n\
             peryx_availability_apply_seconds_count {count}",
            count = state.latency.count,
            sum = state.latency.sum_seconds,
        );
    }
}

#[cfg(test)]
#[path = "../tests/unit/runtime_metrics/tests.rs"]
mod tests;
