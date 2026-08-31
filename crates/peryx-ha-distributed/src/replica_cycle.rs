//! One replica synchronization cycle's complete result.
//!
//! A replica's readiness answer combines a metadata outcome, a blob outcome and the readable
//! frontier. Publishing those one at a time lets a probe pair a fresh metadata serial with a
//! frontier from the previous pass and report a node ready to serve state it cannot read. The loop
//! assembles this value while the cycle runs and publishes it once, so the readiness probe, the
//! caught-up gauge and the sync metrics all describe the same pass.

use std::time::Duration;

use crate::{BlobPlaneReport, RetiredPeer, SyncError, SyncOutcome};

/// Why the metadata plane could not advance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataFault {
    /// The local metadata store could not answer.
    Store,
    /// The primary lost, rejected, or malformed this cycle's page.
    Transport,
    /// The primary speaks a replication protocol this replica cannot apply.
    IncompatibleSchema,
}

impl MetadataFault {
    pub(crate) const fn of(error: &SyncError) -> Self {
        match error {
            SyncError::UnsupportedVersion { .. } => Self::IncompatibleSchema,
            SyncError::Store(_) => Self::Store,
            _ => Self::Transport,
        }
    }

    /// The readiness reason an operator and a load balancer read for this fault.
    pub(crate) const fn reason(self) -> &'static str {
        match self {
            Self::Store => "metadata_store",
            Self::Transport => "sync_error",
            Self::IncompatibleSchema => "incompatible_schema",
        }
    }
}

/// What the blob plane did this cycle.
pub enum BlobPass {
    /// The metadata plane failed first, so the blob plane never ran and an earlier fault stands.
    Skipped,
    Completed(BlobPlaneReport),
    Failed(SyncError),
}

/// The peer retirement this cycle observed.
pub struct RetiredPeers {
    pub peers: Vec<RetiredPeer>,
    pub fully_retired: bool,
}

pub struct ReplicaCycle {
    pub metadata: Result<SyncOutcome, SyncError>,
    pub blobs: BlobPass,
    /// The lowest serial every required derived view has applied, read after both planes ran.
    pub readable: u64,
    /// `None` when the cycle never reached the peer set, leaving the previous retirement standing.
    pub retired: Option<RetiredPeers>,
    /// End-to-end duration of the cycle, counted once whatever mix of outcomes it carried.
    pub elapsed: Duration,
}
