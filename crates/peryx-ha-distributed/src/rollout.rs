use std::collections::BTreeSet;

use crate::readiness::{DurabilityPolicy, MemberFrontier, MemberRole, ReadinessBlocker, group_readiness};
use crate::upgrade::{Preflight, PreflightBlocker, UpgradeTarget, upgrade_preflight};
use crate::versions::{AvailabilityVersions, Version};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RolloutBudget {
    /// Minimum members reporting a frontier after one member drains.
    pub min_serving_after_drain: usize,
    /// Maximum serial gap from the writer frontier to the durable frontier.
    pub max_replication_lag: u64,
    /// Maximum serial gap from the writer frontier to the backup frontier.
    pub max_backup_lag: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RolloutBlocker {
    Version(PreflightBlocker),
    Quorum(ReadinessBlocker),
    Capacity {
        serving_after_drain: usize,
        required: usize,
    },
    ReplicationLag {
        lag: u64,
        allowed: u64,
    },
    BackupLag {
        lag: u64,
        allowed: u64,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum RolloutPreflight {
    Ready,
    Blocked(Vec<RolloutBlocker>),
}

impl RolloutPreflight {
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// Reports version, quorum, capacity, replication-lag, and backup-lag blockers in that order. Version
/// blockers retain [`upgrade_preflight`] order.
///
/// Member IDs are deduplicated with first occurrence winning. The highest reporting writer frontier
/// anchors both lag checks; without a reporting writer, the frontier is zero.
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

/// Orders replicas before writers, sorting each role by member ID, so the serial authority moves once
/// at the end.
///
/// Member IDs are deduplicated with first occurrence winning. Reported frontiers do not affect order.
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

fn deduplicated(members: &[MemberFrontier]) -> Vec<&MemberFrontier> {
    let mut seen = BTreeSet::new();
    members
        .iter()
        .filter(|member| seen.insert(member.member.as_str()))
        .collect()
}
