use std::fmt::Write as _;
use std::sync::Mutex;

use peryx_driver::PrometheusSource;

use crate::{BlobPlaneReport, SyncError, SyncOutcome};

#[derive(Clone, Copy)]
enum ReplicaHealthStatus {
    Starting,
    CatchingUp,
    CaughtUp,
    Error,
}

/// Why a replica is unready, kept apart from the transient status so a schema mismatch a restart
/// cannot resolve reads differently from a page a later poll will drain.
#[derive(Clone, Copy)]
enum ReplicaFault {
    None,
    Sync,
    IncompatibleSchema,
}

#[derive(Clone, Copy)]
pub struct ReplicaObservation {
    status: ReplicaHealthStatus,
    fault: ReplicaFault,
    pub serial: u64,
    pub primary_serial: Option<u64>,
    pub changes: u64,
    pub errors: u64,
    readable_serial: u64,
    blobs_fetched: u64,
    blobs_pending: u64,
}

pub struct ReplicaMonitor {
    observation: Mutex<ReplicaObservation>,
}

impl ReplicaMonitor {
    #[must_use]
    pub const fn new(serial: u64) -> Self {
        Self {
            observation: Mutex::new(ReplicaObservation {
                status: ReplicaHealthStatus::Starting,
                fault: ReplicaFault::None,
                serial,
                primary_serial: None,
                changes: 0,
                errors: 0,
                readable_serial: 0,
                blobs_fetched: 0,
                blobs_pending: 0,
            }),
        }
    }

    pub(crate) fn record(&self, outcome: SyncOutcome) {
        let mut observation = self
            .observation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        observation.status = if outcome.caught_up() {
            ReplicaHealthStatus::CaughtUp
        } else {
            ReplicaHealthStatus::CatchingUp
        };
        observation.fault = ReplicaFault::None;
        observation.serial = outcome.serial;
        observation.primary_serial = Some(outcome.primary_serial);
        observation.changes = observation
            .changes
            .saturating_add(u64::try_from(outcome.changes).unwrap_or(u64::MAX));
    }

    /// Record the serial a reader may safely serve, the lowest frontier every required derived view
    /// has applied. It trails the committed serial while the search index catches up, so a scrape
    /// shows how far derived views lag the applied metadata.
    pub(crate) fn record_readable(&self, serial: u64) {
        let mut observation = self
            .observation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        observation.readable_serial = serial;
    }

    /// Record what one blob-plane pass made of its outstanding blobs: add the blobs it committed to the
    /// cumulative fetched counter and set the pending gauge to how many it left for a later pass. A backlog
    /// that the plane cannot drain shows as a pending gauge stuck above zero while the readable serial
    /// stalls, so a stuck blob is visible before the readable frontier lags.
    pub(crate) fn record_blobs(&self, report: BlobPlaneReport) {
        let mut observation = self
            .observation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        observation.blobs_fetched = observation
            .blobs_fetched
            .saturating_add(u64::try_from(report.fetched).unwrap_or(u64::MAX));
        observation.blobs_pending = u64::try_from(report.pending).unwrap_or(u64::MAX);
    }

    pub(crate) fn record_error(&self, error: &SyncError) {
        let mut observation = self
            .observation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        observation.status = ReplicaHealthStatus::Error;
        observation.fault = match error {
            SyncError::UnsupportedVersion { .. } => ReplicaFault::IncompatibleSchema,
            _ => ReplicaFault::Sync,
        };
        observation.errors = observation.errors.saturating_add(1);
    }

    pub fn snapshot(&self) -> ReplicaObservation {
        *self
            .observation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The reason this replica cannot yet serve at the primary's frontier, or `None` when it is
    /// caught up and error-free. A persistent schema mismatch outranks a transient sync failure,
    /// which outranks ordinary catch-up lag.
    pub fn readiness_gap(&self) -> Option<&'static str> {
        let observation = self.snapshot();
        match observation.fault {
            ReplicaFault::IncompatibleSchema => Some("incompatible_schema"),
            ReplicaFault::Sync => Some("sync_error"),
            ReplicaFault::None => match observation.status {
                ReplicaHealthStatus::CaughtUp => None,
                ReplicaHealthStatus::Starting | ReplicaHealthStatus::CatchingUp | ReplicaHealthStatus::Error => {
                    Some("frontier_lag")
                }
            },
        }
    }
}

impl PrometheusSource for ReplicaMonitor {
    fn write_metrics(&self, body: &mut String) {
        let observation = *self
            .observation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let caught_up = u8::from(matches!(observation.status, ReplicaHealthStatus::CaughtUp));
        let _ = write!(
            body,
            "# HELP peryx_ha_distributed_caught_up Whether the replica has reached the latest observed primary serial.\n\
             # TYPE peryx_ha_distributed_caught_up gauge\n\
             peryx_ha_distributed_caught_up {caught_up}\n\
             # HELP peryx_ha_distributed_serial Last serial committed by the replica.\n\
             # TYPE peryx_ha_distributed_serial gauge\n\
             peryx_ha_distributed_serial {}\n\
             # HELP peryx_ha_distributed_changes_total Metadata changes committed by the replica.\n\
             # TYPE peryx_ha_distributed_changes_total counter\n\
             peryx_ha_distributed_changes_total {}\n\
             # HELP peryx_ha_distributed_sync_errors_total Replica synchronization failures.\n\
             # TYPE peryx_ha_distributed_sync_errors_total counter\n\
             peryx_ha_distributed_sync_errors_total {}\n",
            observation.serial, observation.changes, observation.errors
        );
        let _ = write!(
            body,
            "# HELP peryx_ha_distributed_readable_serial Highest serial every required derived view has applied.\n\
             # TYPE peryx_ha_distributed_readable_serial gauge\n\
             peryx_ha_distributed_readable_serial {}\n\
             # HELP peryx_ha_distributed_blobs_fetched_total Blobs the dual-plane blob fetch committed.\n\
             # TYPE peryx_ha_distributed_blobs_fetched_total counter\n\
             peryx_ha_distributed_blobs_fetched_total {}\n\
             # HELP peryx_ha_distributed_blobs_pending Blobs the last blob-plane pass left outstanding.\n\
             # TYPE peryx_ha_distributed_blobs_pending gauge\n\
             peryx_ha_distributed_blobs_pending {}\n",
            observation.readable_serial, observation.blobs_fetched, observation.blobs_pending
        );
        if let Some(primary_serial) = observation.primary_serial {
            let _ = write!(
                body,
                "# HELP peryx_ha_distributed_primary_serial Latest serial reported by the primary.\n\
                 # TYPE peryx_ha_distributed_primary_serial gauge\n\
                 peryx_ha_distributed_primary_serial {primary_serial}\n\
                 # HELP peryx_ha_distributed_lag Serial distance between the primary and replica.\n\
                 # TYPE peryx_ha_distributed_lag gauge\n\
                 peryx_ha_distributed_lag {}\n",
                primary_serial.saturating_sub(observation.serial)
            );
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/replica_monitor/tests.rs"]
mod tests;
