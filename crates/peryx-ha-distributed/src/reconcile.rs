//! Classifying and reconciling a durable old-epoch operation after authority transfer.
//!
//! Once a newer epoch commits, the [authority fence](crate::AuthorityFence) stops the old home from
//! applying more work, but operations it durably recorded before the transfer still sit in the log and
//! need a disposition. [`classify`] returns the single terminal category one operation falls into, and
//! [`reconcile`] wires that category into the action the reconciler takes: a settled operation with
//! nothing to re-apply, or a [`ReplayCommand`] that re-issues a still-standing operation under the new
//! epoch while preserving its original audit identity. [`cleanup`] gates releasing a reconciled record
//! on the replica and retention frontiers, so no lagging replica or in-window audit read loses it.
//!
//! The classification and the single-operation wiring are pure; [`drain_reconcile`] applies them to the
//! durable backlog the [`MetaStore`] holds, settling each pending old-epoch operation to its terminal
//! outcome. The drain is restart-safe by construction: a settled entry is never re-run, so a process
//! that stops mid-backlog resumes and settles each operation exactly once. Deriving the [`OldEpochOp`]
//! facts from the committed operation record and current metadata is the reconciler's producer, above
//! this core.

use peryx_storage::meta::{MetaError, MetaStore};

use crate::envelope::{AuthorityEpoch, TraceError, derive_child};

/// What the reconciler has derived about one durable old-epoch operation, from the committed operation
/// record, its epoch, and the current metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OldEpochOp {
    /// The operation reached a durable commit before authority transferred. `false` means it was in
    /// flight and never committed, so it has no effect to keep.
    pub durably_committed: bool,
    /// The current committed state already carries this operation's effect, so applying it again would
    /// be a no-op.
    pub already_applied: bool,
    /// A newer committed operation has overwritten this operation's target, so its intent no longer
    /// stands.
    pub superseded: bool,
}

/// The single terminal outcome an old-epoch operation reconciles to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disposition {
    /// Its effect is already in the committed state; reconciliation is a no-op.
    AlreadyApplied,
    /// Durable and still standing; the reconciler replays it under the new epoch.
    Replayable,
    /// A newer operation has overwritten its target; it is dropped as obsolete.
    Superseded,
    /// It never reached a durable commit; it terminates as failed with nothing to apply.
    Failed,
}

impl Disposition {
    /// The stable machine token recorded for this outcome, so a settled backlog entry names its terminal
    /// category in a form the store and an operator query read without the enum.
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

/// Classify `op` into the one terminal [`Disposition`] it reconciles to.
///
/// The precedence is fixed so the outcome is deterministic and single-valued: an operation that never
/// committed fails outright, ahead of every other consideration; an already-applied operation is a
/// no-op even when a later operation also superseded it, because idempotency has already settled its
/// effect; a durable, unapplied operation whose target a newer operation overwrote is superseded; and a
/// durable, unapplied, unsuperseded operation still stands and is replayable.
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

/// The audit identity of one durable old-epoch operation the reconciler acts on.
///
/// Names where it came from, the epoch it committed under, its serial, and the W3C traceparent it
/// authored, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OldEpochIdentity<'a> {
    pub source: &'a str,
    /// The epoch the operation originally committed under, before the transfer.
    pub epoch: AuthorityEpoch,
    pub serial: u64,
    /// The traceparent the original operation carried, so a replay can continue its trace; `None` when
    /// the operation authored no trace context.
    pub traceparent: Option<&'a str>,
}

/// A [`Replayable`](Disposition::Replayable) operation re-issued under the new committed epoch.
///
/// It keeps the original operation's source and serial as its audit identity and continues the original
/// trace, so the replay stays linkable to the operation it re-applies and idempotent under the same
/// serial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayCommand {
    pub source: String,
    /// The epoch the operation originally committed under, retained so the replay records what it
    /// re-applies.
    pub from_epoch: AuthorityEpoch,
    /// The current committed epoch the replay is issued under.
    pub epoch: AuthorityEpoch,
    pub serial: u64,
    /// The derived-child traceparent that continues the original trace, or `None` when the original
    /// carried no trace context.
    pub traceparent: Option<String>,
}

/// The action the reconciler takes for one classified old-epoch operation: the wiring [`classify`]'s
/// disposition drives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconcileAction {
    /// The operation reached a terminal outcome with nothing to re-apply: its effect already stands, a
    /// newer operation superseded it, or it never committed. Carries that disposition for the audit
    /// record.
    Settle(Disposition),
    /// The operation still stands and is re-issued under the new epoch.
    Replay(ReplayCommand),
}

/// Reconcile one durable old-epoch operation into the action that settles it under the new committed
/// `epoch`.
///
/// Classifies `op`, then wires the outcome: an [`AlreadyApplied`](Disposition::AlreadyApplied),
/// [`Superseded`](Disposition::Superseded), or [`Failed`](Disposition::Failed) operation settles with
/// nothing to re-apply, while a [`Replayable`](Disposition::Replayable) one is re-issued under `epoch`
/// as a [`ReplayCommand`] that keeps the original source and serial and continues the original trace, so
/// the replay preserves the operation's audit identity and stays idempotent under the same serial.
/// `span_id` is the fresh child span the replay's traceparent takes, supplied by the caller so the
/// derivation is deterministic.
///
/// # Errors
/// Returns [`TraceError`] when a replayable operation carries a traceparent that is not well-formed or
/// `span_id` is not a valid span id, so a malformed trace never yields a replay that would break the
/// audit trail it is meant to preserve.
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

/// Whether a reconciled operation's record may be released or must be retained.
///
/// The verdict follows how far the required replicas and the operator audit retention have each
/// advanced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cleanup {
    /// A replica or the audit window still trails the operation, so its record is kept.
    Retain,
    /// Both frontiers have passed the operation, so no replica catch-up or audit read can still need it.
    Release,
}

impl Cleanup {
    /// Whether the record may be released.
    #[must_use]
    pub const fn is_release(self) -> bool {
        matches!(self, Self::Release)
    }
}

/// Decide whether a reconciled operation at `serial` may be cleaned up.
///
/// A record is released only once both the required-replica frontier and the operator audit-retention
/// frontier have reached its serial, so a lagging replica that has not yet applied it, or an audit query
/// still inside its retention window, always finds the record. The bound is inclusive: an operation
/// exactly at a frontier is covered by it.
#[must_use]
pub const fn cleanup(serial: u64, replica_frontier: u64, retention_frontier: u64) -> Cleanup {
    if serial <= replica_frontier && serial <= retention_frontier {
        Cleanup::Release
    } else {
        Cleanup::Retain
    }
}

/// How one bounded drain of the reconciliation backlog resolved, tallied by terminal disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReconcileDrain {
    pub already_applied: usize,
    pub replayable: usize,
    pub superseded: usize,
    pub failed: usize,
}

impl ReconcileDrain {
    /// The total number of operations this drain settled.
    #[must_use]
    pub const fn settled(&self) -> usize {
        self.already_applied + self.replayable + self.superseded + self.failed
    }
}

/// Drain up to `limit` pending old-epoch operations from `meta`'s reconciliation backlog, classifying
/// each from its recorded facts and stamping its terminal outcome, and report the tally by disposition.
///
/// The drain is restart-safe: [`settle_reconcile`](MetaStore::settle_reconcile) never re-settles an
/// entry already terminal, so a drain repeated after a process restart resumes from the still-pending
/// entries and settles each operation exactly once. It counts only the entries it moved, so a repeated
/// or overlapping drain never double-counts. `limit` bounds the scan and the write batch.
///
/// Re-issuing a [`Replayable`](Disposition::Replayable) operation under the new epoch is a separate step
/// the caller drives through [`reconcile`], which preserves the operation's audit identity; this drain
/// records the classification the backlog retains until cleanup.
///
/// # Errors
/// Returns the [`MetaError`] when the backlog cannot be read or an outcome cannot be committed.
pub fn drain_reconcile(meta: &MetaStore, limit: usize, now: i64) -> Result<ReconcileDrain, MetaError> {
    let mut report = ReconcileDrain::default();
    for (key, entry) in meta.pending_reconcile(limit)? {
        let disposition = classify(&OldEpochOp {
            durably_committed: entry.durably_committed,
            already_applied: entry.already_applied,
            superseded: entry.superseded,
        });
        // `pending_reconcile` returned only unsettled entries, so settling moves each one; a drain
        // repeated after a restart finds them already terminal and skips them, which is what keeps the
        // tally exactly once across restarts.
        meta.settle_reconcile(&key, disposition.code(), now)?;
        match disposition {
            Disposition::AlreadyApplied => report.already_applied += 1,
            Disposition::Replayable => report.replayable += 1,
            Disposition::Superseded => report.superseded += 1,
            Disposition::Failed => report.failed += 1,
        }
    }
    Ok(report)
}
