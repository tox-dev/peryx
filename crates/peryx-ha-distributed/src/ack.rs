//! Deciding whether one write is durably acknowledged from a datacenter group's readiness.
//!
//! [`group_readiness`](crate::group_readiness) folds a group's member frontiers into readiness and a
//! durable frontier; [`acknowledge`] consumes that evidence to decide a single write. A write at
//! metadata serial `target` is acknowledged once the group is ready and its durable frontier has reached
//! `target`, so the decision is a read of [`GroupReadiness`] against the write's serial with no
//! transport, loop, clock, or storage. The durability the group proves for the artifact bytes is a
//! separate dimension the caller composes on top when that layer exists.

use crate::readiness::{GroupReadiness, ReadinessBlocker};

/// The outcome of deciding whether a write is durably acknowledged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckDecision {
    /// The group is ready and its durable frontier covers the write's serial, so the write is durable.
    Acknowledged,
    /// The group cannot acknowledge any write yet; it carries the blocker that holds readiness back.
    NotReady(ReadinessBlocker),
    /// The group is ready but its durable frontier has not reached the write's serial, so the write is
    /// applied without a durability guarantee.
    NotYetDurable { target: u64, durable_frontier: u64 },
}

impl AckDecision {
    /// Whether the write is durably acknowledged.
    #[must_use]
    pub const fn is_acknowledged(self) -> bool {
        matches!(self, Self::Acknowledged)
    }
}

/// Decide whether a write at metadata serial `target` is durably acknowledged given the group's
/// `evidence`.
///
/// A blocked group cannot acknowledge any write, so it yields [`NotReady`](AckDecision::NotReady)
/// carrying its blocker before the frontier matters. A ready group acknowledges once its durable
/// frontier has reached `target`; the boundary is inclusive, so a write exactly at the frontier is
/// [`Acknowledged`](AckDecision::Acknowledged). A ready group whose frontier still trails `target`
/// yields [`NotYetDurable`](AckDecision::NotYetDurable).
#[must_use]
pub const fn acknowledge(evidence: &GroupReadiness, target: u64) -> AckDecision {
    match evidence.blocked {
        Some(blocker) => AckDecision::NotReady(blocker),
        None if target <= evidence.durable_frontier => AckDecision::Acknowledged,
        None => AckDecision::NotYetDurable {
            target,
            durable_frontier: evidence.durable_frontier,
        },
    }
}
