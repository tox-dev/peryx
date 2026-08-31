use std::fmt::Write as _;
use std::sync::Mutex;

use peryx_core::PrometheusSource;

use crate::RetiredPeer;
use crate::replica_cycle::{BlobPass, MetadataFault, ReplicaCycle};

/// One replica cycle's published result.
///
/// Every field comes from the same cycle, so a reader that takes one snapshot sees a metadata
/// outcome, a blob outcome and a readable frontier that agree with one another.
#[derive(Clone)]
pub struct ReplicaObservation {
    metadata_fault: Option<MetadataFault>,
    /// A failed blob pass sets it and only a later complete pass clears it; a metadata outcome
    /// leaves it standing.
    blob_fault: bool,
    pub serial: u64,
    pub primary_serial: Option<u64>,
    pub changes: u64,
    pub errors: u64,
    /// The lowest serial every required derived view has applied. Reads never pass it, so it - not
    /// the metadata serial - is the frontier readiness compares against the primary.
    pub readable_serial: u64,
    blobs_fetched: u64,
    blobs_pending: u64,
    pub retired: Vec<RetiredPeer>,
    pub fully_retired: bool,
}

impl ReplicaObservation {
    /// Every reason this replica cannot serve the primary's latest observed serial.
    ///
    /// A blob-plane fault and a lagging derived view stay distinct from a metadata transport
    /// failure: the metadata plane can be current while reads still have to hold back.
    #[must_use]
    pub fn readiness_gaps(&self) -> Vec<&'static str> {
        let mut gaps = Vec::new();
        gaps.extend(self.metadata_fault.map(MetadataFault::reason));
        if self.fully_retired {
            gaps.push("retired_peers");
        }
        if self.blob_fault {
            gaps.push("blob_plane");
        }
        gaps.extend(match self.primary_serial {
            None => Some("frontier_lag"),
            Some(primary) if self.serial < primary => Some("frontier_lag"),
            Some(primary) if self.readable_serial < primary => Some("readable_lag"),
            Some(_) => None,
        });
        gaps
    }

    /// Ready means every required derived view reflects the latest serial the primary reported and
    /// no plane is faulted. The readiness probe and the caught-up gauge share this condition.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.readiness_gaps().is_empty()
    }
}

pub struct ReplicaMonitor {
    observation: Mutex<ReplicaObservation>,
}

impl ReplicaMonitor {
    #[must_use]
    pub const fn new(serial: u64) -> Self {
        Self {
            observation: Mutex::new(ReplicaObservation {
                metadata_fault: None,
                blob_fault: false,
                serial,
                primary_serial: None,
                changes: 0,
                errors: 0,
                readable_serial: 0,
                blobs_fetched: 0,
                blobs_pending: 0,
                retired: Vec::new(),
                fully_retired: false,
            }),
        }
    }

    /// Applies one cycle under a single lock. Publishing plane by plane would let a probe between
    /// two of them read a metadata outcome beside a blob outcome from the previous pass.
    pub(crate) fn publish(&self, cycle: ReplicaCycle) {
        let mut observation = self
            .observation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match cycle.metadata {
            Ok(outcome) => {
                observation.metadata_fault = None;
                observation.serial = outcome.serial;
                observation.primary_serial = Some(outcome.primary_serial);
                observation.changes = observation
                    .changes
                    .saturating_add(u64::try_from(outcome.changes).unwrap_or(u64::MAX));
            }
            Err(error) => {
                observation.metadata_fault = Some(MetadataFault::of(&error));
                observation.errors = observation.errors.saturating_add(1);
            }
        }
        match cycle.blobs {
            BlobPass::Completed(report) => {
                observation.blob_fault = false;
                observation.blobs_fetched = observation
                    .blobs_fetched
                    .saturating_add(u64::try_from(report.fetched).unwrap_or(u64::MAX));
                observation.blobs_pending = u64::try_from(report.pending).unwrap_or(u64::MAX);
            }
            BlobPass::Failed(_) => {
                observation.blob_fault = true;
                observation.errors = observation.errors.saturating_add(1);
            }
            BlobPass::Skipped => {}
        }
        observation.readable_serial = cycle.readable;
        if let Some(retired) = cycle.retired {
            observation.retired = retired.peers;
            observation.fully_retired = retired.fully_retired;
        }
    }

    pub fn snapshot(&self) -> ReplicaObservation {
        self.observation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl PrometheusSource for ReplicaMonitor {
    fn write_metrics(&self, body: &mut String) {
        let observation = self.snapshot();
        let caught_up = u8::from(observation.is_ready());
        let _ = write!(
            body,
            "# HELP peryx_ha_distributed_caught_up Whether every required view reflects the latest observed primary serial with no plane faulted.\n\
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
