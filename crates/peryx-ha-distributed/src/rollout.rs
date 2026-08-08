//! Deciding whether a rolling availability upgrade may take its next node out of service now.
//!
//! [`upgrade_preflight`](crate::upgrade_preflight) settles the version axis of a rolling upgrade: that
//! every committed voter runs the target and the target sits at or above the irreversible-migration
//! floor. Draining a node to replace it also has to be safe operationally. The group must keep quorum,
//! keep enough members serving once the node leaves, and be caught up enough that a failed step rolls
//! back from a current replica set and a recent backup. This module folds those operational rules
//! together with the version verdict into one operator-facing go decision, [`rollout_preflight`], and
//! fixes the order a rolling upgrade replaces members in, [`upgrade_order`].
//!
//! Both are pure decisions over measured inputs - frontiers, versions, and operator budgets - so like
//! the rest of the version layer nothing here reaches for transport, a clock, or storage. Measuring the
//! frontiers and acting on the verdict is the rollout job's wiring.
//!
//! A verdict names every unmet rule in a fixed order - version rules first (in their own evaluation
//! order), then quorum, capacity, replication lag, and backup currency - so it is deterministic and an
//! operator sees every reason to wait at once.

use std::collections::BTreeSet;

use crate::readiness::{DurabilityPolicy, MemberFrontier, MemberRole, ReadinessBlocker, group_readiness};
use crate::upgrade::{Preflight, PreflightBlocker, UpgradeTarget, upgrade_preflight};
use crate::versions::{AvailabilityVersions, Version};

/// The operator budgets a rolling upgrade must clear before it drains a member for replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RolloutBudget {
    /// The fewest members that must stay reporting once this step drains one, so the group keeps the
    /// capacity to serve reads and reach quorum through the step.
    pub min_serving_after_drain: usize,
    /// The largest serial gap between the writer's frontier and the group's durable frontier tolerated
    /// before draining, so a replica the group promotes mid-roll starts from an almost-current state.
    pub max_replication_lag: u64,
    /// The largest serial gap between the writer's frontier and the backup's applied frontier tolerated,
    /// so a failed step can be recovered from a backup no further behind than this.
    pub max_backup_lag: u64,
}

/// One rule a [`rollout_preflight`] found unmet, so a rolling upgrade must wait before draining a member.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RolloutBlocker {
    /// A version rule does not clear: the target is unsupported by a committed voter, or it rolls back
    /// below the irreversible-migration floor.
    Version(PreflightBlocker),
    /// The group cannot acknowledge a new write at its durability policy, so it has no quorum to drain
    /// into.
    Quorum(ReadinessBlocker),
    /// Draining one member would leave fewer than the budget requires still serving.
    Capacity {
        serving_after_drain: usize,
        required: usize,
    },
    /// The group's durable frontier trails the writer by more than the budget allows.
    ReplicationLag { lag: u64, allowed: u64 },
    /// The backup trails the writer by more than the budget allows, so a failed step could not recover a
    /// current state.
    BackupLag { lag: u64, allowed: u64 },
}

/// The verdict of a rollout preflight.
#[derive(Debug, PartialEq, Eq)]
pub enum RolloutPreflight {
    /// Every rule holds, so the rolling upgrade may drain its next member.
    Ready,
    /// One or more rules fail, each named in evaluation order.
    Blocked(Vec<RolloutBlocker>),
}

impl RolloutPreflight {
    /// Whether every rule holds.
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// Decide whether a rolling upgrade to `target` may drain its next member now.
///
/// `membership` and `irreversible_floor` feed the version gate exactly as
/// [`upgrade_preflight`](crate::upgrade_preflight) reads them. `members` is the group's configured
/// roster with each member's currently reported frontier, `policy` its durability policy, and
/// `backup_applied` the highest serial the backup has stored. The writer's frontier - the highest serial
/// the writer reports, since the writer is the sole source of serials and bounds every member - anchors
/// both lag checks; when no writer reports it reads zero, and the quorum rule already names the lost
/// writer as the real fault.
///
/// The roster is deduplicated by member id, first occurrence winning, so a node listed twice can inflate
/// neither the serving count nor the writer frontier past the real group, matching the invariant
/// [`group_readiness`](crate::group_readiness) upholds.
#[must_use]
pub fn rollout_preflight(
    target: UpgradeTarget,
    membership: &[AvailabilityVersions],
    irreversible_floor: Version,
    members: &[MemberFrontier],
    policy: DurabilityPolicy,
    backup_applied: u64,
    budget: RolloutBudget,
) -> RolloutPreflight {
    let mut blockers = Vec::new();
    if let Preflight::Blocked(version_blockers) = upgrade_preflight(target, membership, irreversible_floor) {
        blockers.extend(version_blockers.into_iter().map(RolloutBlocker::Version));
    }

    let readiness = group_readiness(members, policy);
    if let Some(reason) = readiness.blocked {
        blockers.push(RolloutBlocker::Quorum(reason));
    }

    let roster = deduplicated(members);
    let serving_after_drain = roster
        .iter()
        .filter(|member| member.applied.is_some())
        .count()
        .saturating_sub(1);
    if serving_after_drain < budget.min_serving_after_drain {
        blockers.push(RolloutBlocker::Capacity {
            serving_after_drain,
            required: budget.min_serving_after_drain,
        });
    }

    let writer_frontier = roster
        .iter()
        .filter(|member| member.role == MemberRole::Writer)
        .filter_map(|member| member.applied)
        .max()
        .unwrap_or(0);
    let lag = writer_frontier.saturating_sub(readiness.durable_frontier);
    if lag > budget.max_replication_lag {
        blockers.push(RolloutBlocker::ReplicationLag {
            lag,
            allowed: budget.max_replication_lag,
        });
    }
    let backup_lag = writer_frontier.saturating_sub(backup_applied);
    if backup_lag > budget.max_backup_lag {
        blockers.push(RolloutBlocker::BackupLag {
            lag: backup_lag,
            allowed: budget.max_backup_lag,
        });
    }

    if blockers.is_empty() {
        RolloutPreflight::Ready
    } else {
        RolloutPreflight::Blocked(blockers)
    }
}

/// The order a rolling upgrade replaces `members`, by member id.
///
/// Replicas are replaced before the writer, each set in stable id order, so the authority that issues
/// serials is the last node touched. Every earlier step then drains a replica the group can lose without
/// an authority handoff, and the single unavoidable handoff happens once at the end rather than repeatedly
/// through the roll. The roster is deduplicated by member id, first occurrence winning, so a node listed
/// twice appears once. The order is over the configured roster, independent of what each member currently
/// reports, so a temporarily unreporting member keeps its place.
#[must_use]
pub fn upgrade_order(members: &[MemberFrontier]) -> Vec<String> {
    let roster = deduplicated(members);
    let mut replicas: Vec<&str> = roster
        .iter()
        .filter(|member| member.role == MemberRole::Replica)
        .map(|member| member.member.as_str())
        .collect();
    let mut writers: Vec<&str> = roster
        .iter()
        .filter(|member| member.role == MemberRole::Writer)
        .map(|member| member.member.as_str())
        .collect();
    replicas.sort_unstable();
    writers.sort_unstable();
    replicas.into_iter().chain(writers).map(str::to_owned).collect()
}

/// The roster with each member id kept once, first occurrence winning, so a node listed twice cannot
/// count twice toward serving capacity, the writer frontier, or the upgrade order.
fn deduplicated(members: &[MemberFrontier]) -> Vec<&MemberFrontier> {
    let mut seen = BTreeSet::new();
    members
        .iter()
        .filter(|member| seen.insert(member.member.as_str()))
        .collect()
}
