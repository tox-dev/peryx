//! The target must reach the source barrier before commit. Commit seals one audit and replays it on
//! retry. Commit rejects a cancelled plan; cancellation rejects a committed plan.

use crate::authority::AuthorityKey;
use crate::envelope::AuthorityEpoch;
use crate::ownership::DatacenterId;

#[derive(Debug, PartialEq, Eq)]
pub struct TransferRequest {
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
    Committed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferAudit {
    pub authority: AuthorityKey,
    pub source: DatacenterId,
    pub target: DatacenterId,
    pub actor: String,
    pub reason: String,
    pub barrier: u64,
    pub epoch: AuthorityEpoch,
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

    /// Seals a ready plan once and returns the original audit on retries.
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

    /// Cancellation is idempotent until commit.
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
