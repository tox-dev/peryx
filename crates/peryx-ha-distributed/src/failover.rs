//! Candidate selection for a home-failover transfer.
//!
//! Liveness aging alone never moves authority. A suspicion is a routing hint, not a decision, and the
//! constraint is deliberate: only a home the tracker has confirmed [`Dead`](Suspicion::Dead) is a
//! failover trigger, and even then the move is a proposal the control quorum commits through Raft before
//! any drain begins. This module is that pure choice: given the failed home's suspicion and the eligible
//! datacenters that could take it, [`FailoverPolicy::select`] returns the single [`Failover`] the
//! proposer acts on.
//!
//! Selecting the target is pure. Feeding it live suspicion from the [liveness tracker](crate::LivenessTracker),
//! committing the chosen [`RecordTransfer`](crate::OwnershipCommand::RecordTransfer) on the control
//! quorum, and draining the retained intents the old home left are the proposer's wiring.

use std::num::NonZeroUsize;

use crate::liveness::Suspicion;
use crate::ownership::DatacenterId;

/// One datacenter the selector may move a failed home to, paired with the liveness the tracker holds for
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The datacenter that could receive the home.
    pub datacenter: DatacenterId,
    /// The liveness the tracker holds for it; only an [`Alive`](Suspicion::Alive) candidate is eligible.
    pub suspicion: Suspicion,
}

/// The single decision a home-failover evaluation reaches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failover {
    /// The home is confirmed dead and this eligible datacenter can take it, so the proposer commits the
    /// transfer to it.
    Transfer(DatacenterId),
    /// The home is confirmed dead but no eligible datacenter is alive to take it, so authority holds and
    /// the old home's writes stay retained until a candidate recovers.
    NoCandidate,
    /// The home is still within tolerance — alive, merely suspect, or never heard from — so authority
    /// does not move. Suspicion alone never transfers a home.
    Hold,
}

/// How a home-failover proposer chooses the datacenter to move a dead home to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailoverPolicy {
    max_candidates: NonZeroUsize,
}

impl FailoverPolicy {
    /// A policy that weighs at most `max_candidates` datacenters per evaluation.
    #[must_use]
    pub const fn new(max_candidates: NonZeroUsize) -> Self {
        Self { max_candidates }
    }

    /// Choose the [`Failover`] for a home whose liveness is `home` against `candidates`.
    ///
    /// Holds unless the home is [`Dead`](Suspicion::Dead): a suspect or still-alive home keeps its
    /// authority, because a suspicion is not a failure and only a committed quorum decision moves a home.
    /// For a dead home it takes the first [`Alive`](Suspicion::Alive) candidate in order, weighing at
    /// most [`max_candidates`](FailoverPolicy::new) so a long roster cannot stall one decision; the
    /// caller orders `candidates` so the choice is deterministic. A candidate that is itself suspect,
    /// dead, or unheard from cannot receive a home.
    #[must_use]
    pub fn select(&self, home: Suspicion, candidates: &[Candidate]) -> Failover {
        if home != Suspicion::Dead {
            return Failover::Hold;
        }
        candidates
            .iter()
            .take(self.max_candidates.get())
            .find(|candidate| candidate.suspicion == Suspicion::Alive)
            .map_or(Failover::NoCandidate, |candidate| {
                Failover::Transfer(candidate.datacenter.clone())
            })
    }
}
