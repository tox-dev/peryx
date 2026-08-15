//! Selects a failover target after the home is confirmed [`Dead`](Suspicion::Dead). The result is a
//! proposal; authority movement requires a control-quorum commit through Raft.

use std::num::NonZeroUsize;

use crate::liveness::Suspicion;
use crate::ownership::DatacenterId;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub datacenter: DatacenterId,
    /// Eligibility requires [`Alive`](Suspicion::Alive).
    pub suspicion: Suspicion,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failover {
    Transfer(DatacenterId),
    /// Authority remains with the failed home until a candidate recovers.
    NoCandidate,
    /// Authority remains with a home that is not confirmed dead.
    Hold,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailoverPolicy {
    max_candidates: NonZeroUsize,
}

impl FailoverPolicy {
    #[must_use]
    pub const fn new(max_candidates: NonZeroUsize) -> Self {
        Self { max_candidates }
    }

    /// For a dead home, selects the first alive candidate within the configured limit. Caller order
    /// determines the target.
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
