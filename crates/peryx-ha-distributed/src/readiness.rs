//! Computes readiness from a complete, fixed membership snapshot. The writer's applied serial bounds all
//! replicas, and absent members remain in the roster so loss cannot shrink the quorum.

pub use peryx_ha::DurabilityPolicy;
use std::cmp::Reverse;
use std::collections::BTreeSet;

use crate::visibility::Frontier as VisibilityFrontier;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberRole {
    /// The sole issuer of journal serials.
    Writer,
    Replica,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberFrontier {
    pub member: String,
    pub role: MemberRole,
    /// `None` contributes no durability; a non-reporting writer also blocks new acknowledgements.
    pub applied: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessBlocker {
    WriterLost,
    InsufficientMembers { reporting: usize, required: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupReadiness {
    pub blocked: Option<ReadinessBlocker>,
    /// Remains valid when [`blocked`](Self::blocked) prevents new acknowledgements.
    pub durable_frontier: u64,
}

impl GroupReadiness {
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        self.blocked.is_none()
    }
}

/// `members` must contain the full configured roster. Missing frontiers use
/// [`MemberFrontier::applied`] `None`. The durable frontier is the required-th largest applied serial.
#[must_use]
pub fn group_readiness(members: &[MemberFrontier], policy: DurabilityPolicy) -> GroupReadiness {
    // Count each physical member once so a duplicate cannot create a quorum.
    let mut seen = BTreeSet::new();
    let roster: Vec<&MemberFrontier> = members
        .iter()
        .filter(|member| seen.insert(member.member.as_str()))
        .collect();

    let required = policy.required_acks(roster.len());
    let reporting = roster.iter().filter(|member| member.applied.is_some()).count();
    let writer_applied = roster
        .iter()
        .find_map(|member| (member.role == MemberRole::Writer).then_some(member.applied).flatten());
    let blocked = if writer_applied.is_none() {
        Some(ReadinessBlocker::WriterLost)
    } else if reporting < required {
        Some(ReadinessBlocker::InsufficientMembers { reporting, required })
    } else {
        None
    };
    // The writer issues all serials; clamp the durable frontier to its report.
    let frontier = durable_frontier(&roster, required);
    let bounded = writer_applied.map_or(frontier, |applied| frontier.min(applied));
    GroupReadiness {
        blocked,
        durable_frontier: bounded,
    }
}

/// Bounds compaction by replicated and backup durability across contiguous authority epochs.
#[must_use]
pub fn visibility_compaction_frontier(
    members: &[MemberFrontier],
    policy: DurabilityPolicy,
    backup_applied: u64,
    epoch: u64,
) -> VisibilityFrontier {
    let replicated = group_readiness(members, policy).durable_frontier;
    let mut frontier = VisibilityFrontier::default();
    for drained in 1..epoch {
        frontier.acknowledge(drained, u64::MAX);
    }
    frontier.acknowledge(epoch, replicated.min(backup_applied));
    frontier
}

/// Returns the required-th largest serial, counting a missing report as zero. Returns zero when the
/// requested quorum exceeds the roster.
fn durable_frontier(members: &[&MemberFrontier], required: usize) -> u64 {
    if required == 0 || required > members.len() {
        return 0;
    }
    let mut applied: Vec<u64> = members.iter().map(|member| member.applied.unwrap_or(0)).collect();
    applied.sort_unstable_by_key(|&serial| Reverse(serial));
    applied[required - 1]
}
