//! Version gates for rolling availability upgrades.
//!
//! Every committed member must support the target. The state-machine target cannot fall below the
//! irreversible-migration floor because older builds cannot restore migrated snapshots.

use crate::versions::{AvailabilityVersions, Version};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpgradeTarget {
    pub protocol: Version,
    pub state_machine: Version,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreflightBlocker {
    TargetUnsupported,
    /// The target state machine cannot restore a snapshot written after an irreversible migration.
    IrreversibleRollback,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Preflight {
    Ready,
    Blocked(Vec<PreflightBlocker>),
}

impl Preflight {
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// Reports target-support blockers before rollback-floor blockers. An empty membership cannot support
/// a target.
#[must_use]
pub fn upgrade_preflight(
    target: UpgradeTarget,
    membership: &[AvailabilityVersions],
    irreversible_floor: Version,
) -> Preflight {
    let mut blockers = Vec::new();
    if !target_supported(target, membership) {
        blockers.push(PreflightBlocker::TargetUnsupported);
    }
    if target.state_machine < irreversible_floor {
        blockers.push(PreflightBlocker::IrreversibleRollback);
    }
    if blockers.is_empty() {
        Preflight::Ready
    } else {
        Preflight::Blocked(blockers)
    }
}

fn target_supported(target: UpgradeTarget, membership: &[AvailabilityVersions]) -> bool {
    !membership.is_empty()
        && membership
            .iter()
            .all(|node| node.protocol.supports(target.protocol) && node.state_machine.supports(target.state_machine))
}
