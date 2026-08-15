use crate::upgrade::PreflightBlocker::{IrreversibleRollback, TargetUnsupported};
use crate::upgrade::{Preflight, UpgradeTarget, upgrade_preflight};
use crate::versions::{AvailabilityVersions, Version, VersionRange};

fn v(number: u16) -> Version {
    Version(number)
}

fn range(min: u16, max: u16) -> VersionRange {
    VersionRange {
        min: Version(min),
        max: Version(max),
    }
}

fn versions(protocol: (u16, u16), state_machine: (u16, u16)) -> AvailabilityVersions {
    AvailabilityVersions {
        protocol: range(protocol.0, protocol.1),
        state_machine: range(state_machine.0, state_machine.1),
    }
}

fn target(protocol: u16, state_machine: u16) -> UpgradeTarget {
    UpgradeTarget {
        protocol: v(protocol),
        state_machine: v(state_machine),
    }
}

fn membership() -> Vec<AvailabilityVersions> {
    vec![versions((1, 3), (1, 3)), versions((1, 3), (1, 3))]
}

#[test]
fn test_a_supported_target_above_the_floor_is_ready() {
    assert_eq!(upgrade_preflight(target(2, 2), &membership(), v(1)), Preflight::Ready);
}

#[test]
fn test_a_target_at_the_floor_is_ready() {
    assert_eq!(upgrade_preflight(target(2, 2), &membership(), v(2)), Preflight::Ready);
}

#[test]
fn test_a_state_machine_target_above_a_members_range_is_unsupported() {
    assert_eq!(
        upgrade_preflight(target(2, 5), &membership(), v(1)),
        Preflight::Blocked(vec![TargetUnsupported])
    );
}

#[test]
fn test_a_state_machine_target_below_a_members_range_is_unsupported() {
    let membership = vec![versions((1, 3), (2, 3))];
    assert_eq!(
        upgrade_preflight(target(2, 1), &membership, v(1)),
        Preflight::Blocked(vec![TargetUnsupported])
    );
}

#[test]
fn test_a_protocol_target_above_a_members_range_is_unsupported() {
    assert_eq!(
        upgrade_preflight(target(5, 2), &membership(), v(1)),
        Preflight::Blocked(vec![TargetUnsupported])
    );
}

#[test]
fn test_an_empty_membership_supports_no_target() {
    assert_eq!(
        upgrade_preflight(target(2, 2), &[], v(1)),
        Preflight::Blocked(vec![TargetUnsupported])
    );
}

#[test]
fn test_a_supported_target_below_the_irreversible_floor_is_a_rollback() {
    assert_eq!(
        upgrade_preflight(target(2, 1), &membership(), v(2)),
        Preflight::Blocked(vec![IrreversibleRollback])
    );
}

#[test]
fn test_both_failing_rules_are_reported_in_evaluation_order() {
    let membership = vec![versions((1, 3), (2, 3))];
    assert_eq!(
        upgrade_preflight(target(2, 1), &membership, v(2)),
        Preflight::Blocked(vec![TargetUnsupported, IrreversibleRollback])
    );
}

#[test]
fn test_is_ready_reflects_the_verdict() {
    assert!(Preflight::Ready.is_ready());
    assert!(!Preflight::Blocked(vec![TargetUnsupported]).is_ready());
}
