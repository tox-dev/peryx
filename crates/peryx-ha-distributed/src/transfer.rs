//! The target must reach the source barrier before commit. Claiming the commit is where cancellation
//! linearizes: a claim rejects a cancelled plan, and cancellation rejects a claimed one. Commit seals one
//! audit and replays it on retry.

use crate::authority::AuthorityKey;
use crate::envelope::AuthorityEpoch;
use crate::ownership::DatacenterId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferRequest {
    /// The one identity this move carries: it keys the replicated control receipt, the audit consensus
    /// sealed, and the retry that resolves against both.
    pub id: String,
    pub authority: AuthorityKey,
    pub source: DatacenterId,
    pub target: DatacenterId,
    pub actor: String,
    pub reason: String,
    /// The metadata serial the target must have applied before the move may commit, so the new home
    /// holds every write the old home acknowledged.
    pub barrier: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferPhase {
    AwaitingCatchUp,
    Ready,
    /// The ownership command is claimed and may already have reached the consensus log, so the move can
    /// no longer be called off.
    Committing,
    Committed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferAudit {
    pub id: String,
    pub authority: AuthorityKey,
    pub source: DatacenterId,
    pub target: DatacenterId,
    pub actor: String,
    pub reason: String,
    pub barrier: u64,
    pub epoch: AuthorityEpoch,
    pub commit_term: u64,
    pub commit_index: u64,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum TransferError {
    /// The target has not replicated through the barrier.
    #[error("the target has not reached the transfer barrier")]
    BarrierNotMet,
    #[error("the transfer already committed and cannot be cancelled")]
    AlreadyCommitted,
    #[error("the transfer was cancelled and cannot commit")]
    Cancelled,
}

#[derive(Debug, PartialEq, Eq)]
enum State {
    AwaitingCatchUp,
    Ready,
    Committing,
    Committed(TransferAudit),
    Cancelled,
}

#[derive(Debug, PartialEq, Eq)]
pub struct TransferPlan {
    request: TransferRequest,
    state: State,
}

impl TransferPlan {
    #[must_use]
    pub const fn plan(request: TransferRequest) -> Self {
        Self {
            request,
            state: State::AwaitingCatchUp,
        }
    }

    #[must_use]
    pub const fn request(&self) -> &TransferRequest {
        &self.request
    }

    #[must_use]
    pub const fn phase(&self) -> TransferPhase {
        match &self.state {
            State::AwaitingCatchUp => TransferPhase::AwaitingCatchUp,
            State::Ready => TransferPhase::Ready,
            State::Committing => TransferPhase::Committing,
            State::Committed(_) => TransferPhase::Committed,
            State::Cancelled => TransferPhase::Cancelled,
        }
    }

    #[must_use]
    pub const fn audit(&self) -> Option<&TransferAudit> {
        match &self.state {
            State::Committed(audit) => Some(audit),
            _ => None,
        }
    }

    /// Advances to ready at the barrier; stale observations cannot move the plan backward.
    pub fn observe_frontier(&mut self, target_applied: u64) -> TransferPhase {
        if matches!(self.state, State::AwaitingCatchUp) && target_applied >= self.request.barrier {
            self.state = State::Ready;
        }
        self.phase()
    }

    /// Claims the commit, the point after which a cancellation is too late. The claim is what lets the
    /// consensus submission run without the plan lock: a cancellation that arrives while the command is
    /// in flight is answered immediately, and answered as refused. A plan already claimed stays claimed,
    /// so a submission whose outcome is unknown is retried under the same transfer identity.
    ///
    /// Returns the sealed audit of a plan that already committed, which submits nothing further.
    ///
    /// # Errors
    /// Returns [`BarrierNotMet`](TransferError::BarrierNotMet) when the target has not reached the
    /// barrier, or [`Cancelled`](TransferError::Cancelled) when the plan was already abandoned.
    pub fn begin_commit(&mut self) -> Result<Option<TransferAudit>, TransferError> {
        match &self.state {
            State::AwaitingCatchUp => Err(TransferError::BarrierNotMet),
            State::Cancelled => Err(TransferError::Cancelled),
            State::Committed(audit) => Ok(Some(audit.clone())),
            State::Ready | State::Committing => {
                self.state = State::Committing;
                Ok(None)
            }
        }
    }

    /// Releases a claim whose command consensus never saw, so the same transfer identity can wait for the
    /// barrier again and stay cancellable. Only a claim is released: a sealed or cancelled plan keeps its
    /// outcome.
    pub fn abandon_commit(&mut self) {
        if matches!(self.state, State::Committing) {
            self.state = State::Ready;
        }
    }

    /// Seals a claimed plan once and returns the original audit on retries.
    ///
    /// # Errors
    /// Returns [`BarrierNotMet`](TransferError::BarrierNotMet) when the commit was never claimed through
    /// [`begin_commit`](Self::begin_commit), or [`Cancelled`](TransferError::Cancelled) when the plan was
    /// already abandoned.
    pub fn commit(
        &mut self,
        epoch: AuthorityEpoch,
        commit_term: u64,
        commit_index: u64,
    ) -> Result<TransferAudit, TransferError> {
        match &self.state {
            State::AwaitingCatchUp | State::Ready => Err(TransferError::BarrierNotMet),
            State::Cancelled => Err(TransferError::Cancelled),
            State::Committed(audit) => Ok(audit.clone()),
            State::Committing => {
                let audit = TransferAudit {
                    id: self.request.id.clone(),
                    authority: self.request.authority.clone(),
                    source: self.request.source.clone(),
                    target: self.request.target.clone(),
                    actor: self.request.actor.clone(),
                    reason: self.request.reason.clone(),
                    barrier: self.request.barrier,
                    epoch,
                    commit_term,
                    commit_index,
                };
                self.state = State::Committed(audit.clone());
                Ok(audit)
            }
        }
    }

    /// Cancellation is idempotent until the commit is claimed.
    ///
    /// # Errors
    /// Returns [`AlreadyCommitted`](TransferError::AlreadyCommitted) once the commit is claimed, so a
    /// cancel that lost the race to it is refused and the move stands.
    pub fn cancel(&mut self) -> Result<(), TransferError> {
        match self.state {
            State::Committing | State::Committed(_) => Err(TransferError::AlreadyCommitted),
            State::Cancelled => Ok(()),
            _ => {
                self.state = State::Cancelled;
                Ok(())
            }
        }
    }
}
