//! Process-global replica-sync metrics use fixed error-class and histogram-bucket labels. Object and
//! topology counts cannot increase series cardinality.

use std::fmt::Write as _;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use crate::replica_cycle::{BlobPass, ReplicaCycle};
use crate::{HeartbeatError, SyncError};
use peryx_core::PrometheusSource;

/// Error text does not affect cardinality because labels use a closed set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncErrorClass {
    Schema,
    Transport,
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

    const fn of(error: &SyncError) -> Self {
        match error {
            SyncError::UnsupportedVersion { .. } => Self::Schema,
            SyncError::Primary(_) => Self::Transport,
            _ => Self::Apply,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeartbeatErrorClass {
    Authentication,
    StaleIncarnation,
    Server,
    Transport,
    Rejected,
}

impl HeartbeatErrorClass {
    const ALL: [Self; 5] = [
        Self::Authentication,
        Self::StaleIncarnation,
        Self::Server,
        Self::Transport,
        Self::Rejected,
    ];

    const fn as_str(self) -> &'static str {
        match self {
            Self::Authentication => "authentication",
            Self::StaleIncarnation => "stale_incarnation",
            Self::Server => "server",
            Self::Transport => "transport",
            Self::Rejected => "rejected",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Authentication => 0,
            Self::StaleIncarnation => 1,
            Self::Server => 2,
            Self::Transport => 3,
            Self::Rejected => 4,
        }
    }

    const fn of(error: &HeartbeatError) -> Self {
        match error {
            HeartbeatError::Authentication { .. } => Self::Authentication,
            HeartbeatError::StaleIncarnation => Self::StaleIncarnation,
            HeartbeatError::Server { .. } => Self::Server,
            HeartbeatError::Transport(_) => Self::Transport,
            HeartbeatError::Rejected { .. } => Self::Rejected,
        }
    }
}

/// Fixed bucket bounds keep cardinality independent of observations. Prometheus exposition derives
/// `+Inf` from the total count.
const LATENCY_BUCKETS_SECONDS: [f64; 6] = [0.005, 0.025, 0.1, 0.5, 2.5, 10.0];

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
    heartbeat_errors: [u64; HeartbeatErrorClass::ALL.len()],
    pending_serials: u64,
    latency: LatencyHistogram,
}

#[derive(Debug, Default)]
pub struct AvailabilityMetrics {
    state: Mutex<AvailabilityState>,
}

impl AvailabilityMetrics {
    fn with<R>(&self, edit: impl FnOnce(&mut AvailabilityState) -> R) -> R {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        edit(&mut state)
    }

    /// Counts one cycle once, whatever mix of outcomes it carried: a pass that advances metadata and
    /// loses the blob plane is a single cycle with a single latency observation.
    ///
    /// A cycle that applied no page leaves pending serials unchanged.
    pub(crate) fn record_cycle(&self, cycle: &ReplicaCycle) {
        let failures = [
            cycle.metadata.as_ref().err(),
            match &cycle.blobs {
                BlobPass::Failed(error) => Some(error),
                BlobPass::Completed(_) | BlobPass::Skipped => None,
            },
        ];
        let pending = cycle
            .metadata
            .as_ref()
            .ok()
            .map(|outcome| outcome.primary_serial.saturating_sub(outcome.serial));
        self.with(|state| {
            state.cycles += 1;
            for class in failures.into_iter().flatten().map(SyncErrorClass::of) {
                state.errors[class.index()] += 1;
            }
            if let Some(pending) = pending {
                state.pending_serials = pending;
            }
            state.latency.observe(cycle.elapsed);
        });
    }

    pub(crate) fn record_heartbeat_error(&self, error: &HeartbeatError) {
        let class = HeartbeatErrorClass::of(error);
        self.with(|state| state.heartbeat_errors[class.index()] += 1);
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
            "# HELP peryx_availability_heartbeat_errors_total Replica heartbeats that failed, by class.\n\
             # TYPE peryx_availability_heartbeat_errors_total counter\n",
        );
        for class in HeartbeatErrorClass::ALL {
            let _ = writeln!(
                body,
                "peryx_availability_heartbeat_errors_total{{class=\"{}\"}} {}",
                class.as_str(),
                state.heartbeat_errors[class.index()]
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
