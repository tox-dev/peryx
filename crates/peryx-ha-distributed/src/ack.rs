//! A ready group acknowledges a write when its durable frontier includes the write's metadata serial.
//! Artifact-byte durability remains a separate decision.

use crate::readiness::{GroupReadiness, ReadinessBlocker};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckDecision {
    Acknowledged,
    NotReady(ReadinessBlocker),
    NotYetDurable { target: u64, durable_frontier: u64 },
}

impl AckDecision {
    #[must_use]
    pub const fn is_acknowledged(self) -> bool {
        matches!(self, Self::Acknowledged)
    }
}

/// A readiness blocker takes precedence over frontier coverage. The durable boundary is inclusive.
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
