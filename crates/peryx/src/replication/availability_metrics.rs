//! Bounded availability metrics for the replica sync loop.
//!
//! The series are process-global on a [read replica](@/core/high-availability.md) and carry no
//! per-object label: `class` on the error counter and `le` on the latency histogram are the only
//! labels, and each draws from a fixed vocabulary. A replicated store therefore adds no series as it
//! grows, so the exposition stays within [`SERIES_BUDGET`] whatever the topology.

use std::fmt::Write as _;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use peryx_driver::PrometheusSource;
use peryx_ha_distributed::{SyncError, SyncOutcome};

/// The number of `peryx_availability_*` series this exporter emits once a replica has run one cycle:
/// the cycle counter, one error counter per [`SyncErrorClass`], the pending-serial gauge, and the
/// latency histogram's buckets, `+Inf`, sum, and count. It is a constant of the metric shape, not of
/// the load, and a test holds the exposition to it.
#[cfg(test)]
const SERIES_BUDGET: usize = 1 + SyncErrorClass::ALL.len() + 1 + LATENCY_BUCKETS_SECONDS.len() + 3;

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

/// The bounded availability counters a replica updates each sync cycle and the `/metrics` endpoint
/// renders. Registered as a [`PrometheusSource`] beside the replica's frontier gauges, it adds the
/// error-class, queue-depth, and latency views without widening any existing series.
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
mod tests {
    use super::*;

    fn rendered(metrics: &AvailabilityMetrics) -> String {
        let mut body = String::new();
        metrics.write_metrics(&mut body);
        body
    }

    #[test]
    fn test_error_class_classifies_each_bounded_reason() {
        assert_eq!(
            SyncErrorClass::of(&SyncError::UnsupportedVersion { actual: 9, expected: 1 }),
            SyncErrorClass::Schema
        );
        assert_eq!(
            SyncErrorClass::of(&SyncError::Primary(Box::new(std::io::Error::other(
                "primary unreachable"
            )))),
            SyncErrorClass::Transport
        );
        assert_eq!(SyncErrorClass::of(&SyncError::EmptySource), SyncErrorClass::Apply);
    }

    #[test]
    fn test_record_error_counts_only_the_matching_class() {
        let metrics = AvailabilityMetrics::default();
        metrics.record_error(
            &SyncError::UnsupportedVersion { actual: 9, expected: 1 },
            Duration::ZERO,
        );
        metrics.record_error(&SyncError::EmptySource, Duration::ZERO);
        metrics.record_error(&SyncError::EmptySource, Duration::ZERO);

        let body = rendered(&metrics);
        assert!(
            body.contains("peryx_availability_sync_errors_total{class=\"schema\"} 1\n"),
            "{body}"
        );
        assert!(
            body.contains("peryx_availability_sync_errors_total{class=\"transport\"} 0\n"),
            "{body}"
        );
        assert!(
            body.contains("peryx_availability_sync_errors_total{class=\"apply\"} 2\n"),
            "{body}"
        );
        assert!(body.contains("peryx_availability_sync_cycles_total 3\n"), "{body}");
    }

    #[test]
    fn test_record_cycle_reports_queue_depth_from_the_frontier_gap() {
        let metrics = AvailabilityMetrics::default();
        metrics.record_cycle(
            SyncOutcome {
                changes: 2,
                serial: 4,
                primary_serial: 7,
            },
            Duration::from_millis(1),
        );
        assert!(rendered(&metrics).contains("peryx_availability_pending_serials 3\n"));

        metrics.record_cycle(
            SyncOutcome {
                changes: 3,
                serial: 7,
                primary_serial: 7,
            },
            Duration::from_millis(1),
        );
        assert!(rendered(&metrics).contains("peryx_availability_pending_serials 0\n"));
    }

    #[test]
    fn test_histogram_buckets_are_cumulative_across_bounds() {
        let metrics = AvailabilityMetrics::default();
        metrics.record_cycle(
            SyncOutcome {
                changes: 0,
                serial: 1,
                primary_serial: 1,
            },
            Duration::from_millis(1),
        );
        metrics.record_cycle(
            SyncOutcome {
                changes: 0,
                serial: 1,
                primary_serial: 1,
            },
            Duration::from_secs(30),
        );

        let body = rendered(&metrics);
        assert!(
            body.contains("peryx_availability_apply_seconds_bucket{le=\"0.005\"} 1\n"),
            "{body}"
        );
        assert!(
            body.contains("peryx_availability_apply_seconds_bucket{le=\"10\"} 1\n"),
            "{body}"
        );
        assert!(
            body.contains("peryx_availability_apply_seconds_bucket{le=\"+Inf\"} 2\n"),
            "{body}"
        );
        assert!(body.contains("peryx_availability_apply_seconds_count 2\n"), "{body}");
    }

    #[test]
    fn test_exposition_stays_within_the_series_budget() {
        let metrics = AvailabilityMetrics::default();
        metrics.record_error(&SyncError::EmptySource, Duration::from_millis(1));

        let body = rendered(&metrics);
        let series = body
            .lines()
            .filter(|line| line.starts_with("peryx_availability"))
            .count();
        assert_eq!(series, SERIES_BUDGET);
        for forbidden in [
            "tenant",
            "project",
            "repository",
            "digest",
            "operation",
            "trace",
            "node",
        ] {
            assert!(
                !body.contains(forbidden),
                "series carries a high-cardinality label: {forbidden}\n{body}"
            );
        }
    }
}
