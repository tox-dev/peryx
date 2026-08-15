//! Replays retain their audit identity under the new epoch. Cleanup waits for replica and audit-retention
//! frontiers. Backlog settlement is restart-safe because the store skips terminal entries.

use std::num::NonZeroUsize;

use peryx_ha::ReconcileStore;

use crate::envelope::{AuthorityEpoch, TraceError, derive_child};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OldEpochOp {
    pub durably_committed: bool,
    pub already_applied: bool,
    pub superseded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    AlreadyApplied,
    Replayable,
    Superseded,
    Failed,
}

impl Disposition {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::AlreadyApplied => "already_applied",
            Self::Replayable => "replayable",
            Self::Superseded => "superseded",
            Self::Failed => "failed",
        }
    }
}

/// Applies `Failed`, `AlreadyApplied`, `Superseded`, then `Replayable` precedence. `AlreadyApplied` wins
/// over `Superseded` because the committed state contains its effect.
#[must_use]
pub const fn classify(op: &OldEpochOp) -> Disposition {
    if !op.durably_committed {
        Disposition::Failed
    } else if op.already_applied {
        Disposition::AlreadyApplied
    } else if op.superseded {
        Disposition::Superseded
    } else {
        Disposition::Replayable
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OldEpochIdentity<'a> {
    pub source: &'a str,
    pub epoch: AuthorityEpoch,
    pub serial: u64,
    pub traceparent: Option<&'a str>,
}

/// Retains the original source, serial, and trace lineage while replaying under a new epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayCommand {
    pub source: String,
    /// Original commit epoch.
    pub from_epoch: AuthorityEpoch,
    /// Replay epoch.
    pub epoch: AuthorityEpoch,
    pub serial: u64,
    /// Child of the original traceparent.
    pub traceparent: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileAction {
    Settle(Disposition),
    Replay(ReplayCommand),
}

/// Replayable operations retain their source and serial and derive a child traceparent from `span_id`.
///
/// # Errors
/// Returns [`TraceError`] when the original traceparent or `span_id` is invalid.
pub fn reconcile(
    op: &OldEpochOp,
    identity: OldEpochIdentity<'_>,
    epoch: AuthorityEpoch,
    span_id: &str,
) -> Result<ReconcileAction, TraceError> {
    match classify(op) {
        Disposition::Replayable => {
            let traceparent = identity
                .traceparent
                .map(|parent| derive_child(parent, span_id))
                .transpose()?;
            Ok(ReconcileAction::Replay(ReplayCommand {
                source: identity.source.to_owned(),
                from_epoch: identity.epoch,
                epoch,
                serial: identity.serial,
                traceparent,
            }))
        }
        settled => Ok(ReconcileAction::Settle(settled)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cleanup {
    Retain,
    Release,
}

impl Cleanup {
    #[must_use]
    pub const fn is_release(self) -> bool {
        matches!(self, Self::Release)
    }
}

/// Releases a record after replica and audit-retention frontiers reach `serial`. The bounds are inclusive.
#[must_use]
pub const fn cleanup(serial: u64, replica_frontier: u64, retention_frontier: u64) -> Cleanup {
    if serial <= replica_frontier && serial <= retention_frontier {
        Cleanup::Release
    } else {
        Cleanup::Retain
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReconcileDrain {
    pub already_applied: usize,
    pub replayable: usize,
    pub superseded: usize,
    pub failed: usize,
}

impl ReconcileDrain {
    #[must_use]
    pub const fn settled(&self) -> usize {
        self.already_applied + self.replayable + self.superseded + self.failed
    }
}

/// Settles at most `limit` pending operations without recounting terminal entries.
///
/// Callers use [`reconcile`] to construct replays from the recorded classifications.
///
/// # Errors
/// Returns the store error from a backlog read or outcome commit.
pub fn drain_reconcile<S: ReconcileStore>(store: &S, limit: usize, now: i64) -> Result<ReconcileDrain, S::Error> {
    let mut report = ReconcileDrain::default();
    for (key, entry) in store.pending_reconcile(limit)? {
        let disposition = classify(&OldEpochOp {
            durably_committed: entry.durably_committed,
            already_applied: entry.already_applied,
            superseded: entry.superseded,
        });
        store.settle_reconcile(&key, disposition.code(), now)?;
        match disposition {
            Disposition::AlreadyApplied => report.already_applied += 1,
            Disposition::Replayable => report.replayable += 1,
            Disposition::Superseded => report.superseded += 1,
            Disposition::Failed => report.failed += 1,
        }
    }
    Ok(report)
}

/// Deletes up to `limit` settled records covered by replica and retention frontiers.
///
/// # Errors
/// Returns a store error when a backlog row cannot be read or removed.
pub fn prune_reconcile<S: ReconcileStore>(
    store: &S,
    replica_frontier: u64,
    retention_frontier: u64,
    limit: NonZeroUsize,
) -> Result<usize, S::Error> {
    let mut removed = 0;
    let mut cursor = None;
    loop {
        let page = store.scan_reconcile(cursor.as_deref(), limit)?;
        for (key, entry) in page.records {
            if !entry.is_pending()
                && cleanup(entry.serial, replica_frontier, retention_frontier).is_release()
                && store.compare_and_remove_reconcile(&key, &entry)?
            {
                removed += 1;
                if removed == limit.get() {
                    return Ok(removed);
                }
            }
        }
        let Some(next) = page.next_cursor else {
            return Ok(removed);
        };
        cursor = Some(next);
    }
}
