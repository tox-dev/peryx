//! Deciding whether a rolling availability upgrade may commit a new operating version.
//!
//! [`negotiate`](crate::negotiate) and [`feature_activated`](crate::feature_activated) settle which
//! versions a pair or a membership can already speak. This module answers the operator's next question:
//! given the versions the committed voters advertise, may the cluster move its operating point to a
//! chosen [`UpgradeTarget`] now? The answer is a pure decision over those inputs, so like the rest of
//! the version layer it reaches for no transport, clock, or storage; the rollout that acts on the
//! verdict lives above it, and the operational readiness of the group — quorum, replication lag, and
//! backup currency — arrives already measured through [`group_readiness`](crate::group_readiness) and
//! the durable frontier it reports.
//!
//! Two rules guard the version axis. Every committed member must already run the target, so a command
//! at the new version never reaches a member that cannot apply it — the barrier
//! [`feature_activated`](crate::feature_activated) enforces for one feature, widened to both dimensions
//! of a whole target. And the target may not fall below the state-machine version at which an
//! irreversible migration ran, because a snapshot written past that point cannot be restored by an
//! older build; that floor is the rollback boundary. A verdict names every rule a target fails, in a
//! fixed order, so it is deterministic and an operator sees each reason to wait at once.

use crate::versions::{AvailabilityVersions, Version};

/// The versions a rolling upgrade wants every committed member to run once it commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpgradeTarget {
    /// The wire protocol version to operate at.
    pub protocol: Version,
    /// The replicated state-machine version to operate at.
    pub state_machine: Version,
}

/// One rule a preflight found a target fails, so a rolling upgrade must wait or be corrected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreflightBlocker {
    /// A committed member does not run the target, so a command at the new version could reach a member
    /// that cannot apply it.
    TargetUnsupported,
    /// The target state machine is below the version an irreversible migration set, so a snapshot past
    /// that floor could not be restored by the older build.
    IrreversibleRollback,
}

/// The verdict of an upgrade preflight.
#[derive(Debug, PartialEq, Eq)]
pub enum Preflight {
    /// Every rule holds, so the upgrade may commit once the group is operationally ready.
    Ready,
    /// One or more rules fail, each named in evaluation order.
    Blocked(Vec<PreflightBlocker>),
}

impl Preflight {
    /// Whether every version rule holds.
    #[must_use]
    pub const fn is_ready(&self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// Decide whether a rolling upgrade to `target` clears the version rules across `membership`.
///
/// `membership` is the committed voting set's advertised versions, and `irreversible_floor` is the
/// lowest state-machine version the cluster may still operate at, the point a documented irreversible
/// migration raised. Target support is checked before the rollback boundary, so the verdict is
/// deterministic. An empty membership supports no target, matching
/// [`feature_activated`](crate::feature_activated).
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

/// Whether every committed member runs both dimensions of `target`. An empty membership supports
/// nothing, so a preflight against no committed voters never reads ready.
fn target_supported(target: UpgradeTarget, membership: &[AvailabilityVersions]) -> bool {
    !membership.is_empty()
        && membership
            .iter()
            .all(|node| node.protocol.supports(target.protocol) && node.state_machine.supports(target.state_machine))
}
