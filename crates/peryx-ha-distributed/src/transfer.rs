//! Planning and auditing a fenced authority transfer: moving a repository's home to a new datacenter
//! only after the target has caught up, and recording who moved it and why.
//!
//! [`OwnershipState`](crate::OwnershipState) commits the move and mints the fencing epoch, but a
//! planned transfer needs a decision before that commit and a record after it. Before: the target must
//! have replicated the source's writes, or the move strands the operations the target has not yet
//! applied. After: an operator asks who transferred an authority, from where, to where, and why, and an
//! auditor needs the barrier the move waited on and the log index that carried it. This module is that
//! lifecycle as a pure state machine over the observed target frontier and the committed move, so it
//! reaches for no transport, clock, or storage; the control plane that submits the commit and the
//! replication loop that reports the frontier sit above it.
//!
//! One transfer resolves to one outcome. A plan waits at
//! [`AwaitingCatchUp`](TransferPhase::AwaitingCatchUp) until the target's applied frontier reaches the
//! barrier, then stands [`Ready`](TransferPhase::Ready) for the caller to commit. A commit seals the
//! [`TransferAudit`] once, and every later commit reads back that same record, so a retry after a
//! leader loss never books a second move. A cancel abandons a plan that has not committed and is refused
//! once it has, so a race between an operator's cancel and the commit resolves to exactly one of them.

use crate::authority::AuthorityKey;
use crate::envelope::AuthorityEpoch;
use crate::ownership::DatacenterId;

/// A requested authority transfer: which authority moves, between which datacenters, on whose order and
/// for what stated reason, and the target frontier the move waits for.
#[derive(Debug, PartialEq, Eq)]
pub struct TransferRequest {
    /// The authority to move, a project or repository home.
    pub authority: AuthorityKey,
    /// The datacenter the authority is homed at now.
    pub source: DatacenterId,
    /// The datacenter to move the authority to.
    pub target: DatacenterId,
    /// The operator who ordered the transfer, carried into the audit.
    pub actor: String,
    /// The operator's stated reason, carried into the audit.
    pub reason: String,
    /// The metadata serial the target must have applied before the move may commit, so the new home
    /// holds every write the old home acknowledged.
    pub barrier: u64,
}

/// Where a planned transfer stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferPhase {
    /// The target has not yet applied through the barrier.
    AwaitingCatchUp,
    /// The target has caught up, so the caller may commit the move.
    Ready,
    /// The move committed and its audit is sealed.
    Committed,
    /// The plan was abandoned before it committed.
    Cancelled,
}

/// The sealed record of a committed transfer, the audit an operator and reconciliation read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferAudit {
    /// The authority that moved.
    pub authority: AuthorityKey,
    /// The datacenter it moved from.
    pub source: DatacenterId,
    /// The datacenter it moved to.
    pub target: DatacenterId,
    /// The operator who ordered the move.
    pub actor: String,
    /// The operator's stated reason.
    pub reason: String,
    /// The frontier the target had to reach before the move committed.
    pub barrier: u64,
    /// The epoch the move minted, which fences the old home's stale-epoch writes.
    pub epoch: AuthorityEpoch,
    /// The index of the Raft log entry that committed the move.
    pub commit_index: u64,
}

/// Why a transfer operation was refused.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum TransferError {
    /// The target has not reached the barrier, so committing would strand its unreplicated writes.
    #[error("the target has not reached the transfer barrier")]
    BarrierNotMet,
    /// The transfer already committed, so it can no longer be cancelled.
    #[error("the transfer already committed and cannot be cancelled")]
    AlreadyCommitted,
    /// The transfer was cancelled, so it can no longer commit.
    #[error("the transfer was cancelled and cannot commit")]
    Cancelled,
}

#[derive(Debug, PartialEq, Eq)]
enum State {
    AwaitingCatchUp,
    Ready,
    Committed(TransferAudit),
    Cancelled,
}

/// A planned authority transfer and its lifecycle from request to a single sealed outcome.
#[derive(Debug, PartialEq, Eq)]
pub struct TransferPlan {
    request: TransferRequest,
    state: State,
}

impl TransferPlan {
    /// Begin a plan for `request`, waiting for the target to reach the barrier.
    #[must_use]
    pub const fn plan(request: TransferRequest) -> Self {
        Self {
            request,
            state: State::AwaitingCatchUp,
        }
    }

    /// The request the plan carries.
    #[must_use]
    pub const fn request(&self) -> &TransferRequest {
        &self.request
    }

    /// Where the plan stands.
    #[must_use]
    pub const fn phase(&self) -> TransferPhase {
        match &self.state {
            State::AwaitingCatchUp => TransferPhase::AwaitingCatchUp,
            State::Ready => TransferPhase::Ready,
            State::Committed(_) => TransferPhase::Committed,
            State::Cancelled => TransferPhase::Cancelled,
        }
    }

    /// The sealed audit once the plan has committed, or `None` before then.
    #[must_use]
    pub const fn audit(&self) -> Option<&TransferAudit> {
        match &self.state {
            State::Committed(audit) => Some(audit),
            _ => None,
        }
    }

    /// Record the target's latest applied frontier and return where the plan now stands.
    ///
    /// A plan waiting for catch-up advances to [`Ready`](TransferPhase::Ready) once the frontier reaches
    /// the barrier. The step is monotonic: a later frontier never pulls a ready or resolved plan back to
    /// waiting, so an observation that arrives out of order cannot un-ready a move.
    pub fn observe_frontier(&mut self, target_applied: u64) -> TransferPhase {
        if matches!(self.state, State::AwaitingCatchUp) && target_applied >= self.request.barrier {
            self.state = State::Ready;
        }
        self.phase()
    }

    /// Seal the transfer under the `epoch` and `commit_index` the committed move minted.
    ///
    /// A ready plan seals its [`TransferAudit`] and returns it. Committing an already-committed plan
    /// returns that same sealed record rather than booking a second move, so a retry after a leader loss
    /// resolves to one outcome.
    ///
    /// # Errors
    /// Returns [`BarrierNotMet`](TransferError::BarrierNotMet) when the target has not reached the
    /// barrier, or [`Cancelled`](TransferError::Cancelled) when the plan was already abandoned.
    pub fn commit(&mut self, epoch: AuthorityEpoch, commit_index: u64) -> Result<TransferAudit, TransferError> {
        match &self.state {
            State::AwaitingCatchUp => Err(TransferError::BarrierNotMet),
            State::Cancelled => Err(TransferError::Cancelled),
            State::Committed(audit) => Ok(audit.clone()),
            State::Ready => {
                let audit = TransferAudit {
                    authority: self.request.authority.clone(),
                    source: self.request.source.clone(),
                    target: self.request.target.clone(),
                    actor: self.request.actor.clone(),
                    reason: self.request.reason.clone(),
                    barrier: self.request.barrier,
                    epoch,
                    commit_index,
                };
                self.state = State::Committed(audit.clone());
                Ok(audit)
            }
        }
    }

    /// Abandon a plan that has not committed.
    ///
    /// Cancelling an already-cancelled plan is a no-op, so a repeated request resolves the same way.
    ///
    /// # Errors
    /// Returns [`AlreadyCommitted`](TransferError::AlreadyCommitted) when the plan already committed, so
    /// a cancel that lost the race to the commit is refused and the move stands.
    pub fn cancel(&mut self) -> Result<(), TransferError> {
        match self.state {
            State::Committed(_) => Err(TransferError::AlreadyCommitted),
            State::Cancelled => Ok(()),
            _ => {
                self.state = State::Cancelled;
                Ok(())
            }
        }
    }
}
