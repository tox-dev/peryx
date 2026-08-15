use crate::readiness::{DurabilityPolicy, MemberFrontier, MemberRole, ReadinessBlocker};
use crate::rollout::{
    RolloutBlocker::{BackupLag, Capacity, Quorum, ReplicationLag, Version},
    RolloutBudget, RolloutPreflight, rollout_preflight, upgrade_order,
};
use crate::upgrade::PreflightBlocker::{IrreversibleRollback, TargetUnsupported};
use crate::upgrade::UpgradeTarget;
use crate::versions::{AvailabilityVersions, Version as WireVersion, VersionRange};

fn v(number: u16) -> WireVersion {
    WireVersion(number)
}

fn versions(protocol: (u16, u16), state_machine: (u16, u16)) -> AvailabilityVersions {
    AvailabilityVersions {
        protocol: VersionRange {
            min: v(protocol.0),
            max: v(protocol.1),
        },
        state_machine: VersionRange {
            min: v(state_machine.0),
            max: v(state_machine.1),
        },
    }
}

fn target(protocol: u16, state_machine: u16) -> UpgradeTarget {
    UpgradeTarget {
        protocol: v(protocol),
        state_machine: v(state_machine),
    }
}

fn node(name: &str, role: MemberRole, applied: Option<u64>) -> MemberFrontier {
    MemberFrontier {
        member: name.to_owned(),
        role,
        applied,
    }
}

fn writer(applied: Option<u64>) -> MemberFrontier {
    node("writer", MemberRole::Writer, applied)
}

fn replica(name: &str, applied: Option<u64>) -> MemberFrontier {
    node(name, MemberRole::Replica, applied)
}

fn budget(min_serving_after_drain: usize, max_replication_lag: u64, max_backup_lag: u64) -> RolloutBudget {
    RolloutBudget {
        min_serving_after_drain,
        max_replication_lag,
        max_backup_lag,
    }
}

fn membership() -> Vec<AvailabilityVersions> {
    vec![versions((1, 3), (1, 3)), versions((1, 3), (1, 3))]
}

fn caught_up() -> Vec<MemberFrontier> {
    vec![writer(Some(10)), replica("b", Some(10))]
}

#[test]
fn test_every_rule_clearing_is_ready() {
    let verdict = rollout_preflight(
        target(2, 2),
        &membership(),
        v(1),
        &caught_up(),
        DurabilityPolicy::Majority,
        10,
        budget(1, 0, 0),
    );

    assert_eq!(verdict, RolloutPreflight::Ready);
}

#[test]
fn test_a_failing_version_rule_surfaces_as_a_version_blocker() {
    let verdict = rollout_preflight(
        target(2, 5),
        &membership(),
        v(1),
        &caught_up(),
        DurabilityPolicy::Majority,
        10,
        budget(1, 0, 0),
    );

    assert_eq!(verdict, RolloutPreflight::Blocked(vec![Version(TargetUnsupported)]));
}

#[test]
fn test_both_version_rules_surface_in_evaluation_order() {
    let membership = vec![versions((1, 3), (2, 3))];
    let verdict = rollout_preflight(
        target(2, 1),
        &membership,
        v(2),
        &caught_up(),
        DurabilityPolicy::Majority,
        10,
        budget(1, 0, 0),
    );

    assert_eq!(
        verdict,
        RolloutPreflight::Blocked(vec![Version(TargetUnsupported), Version(IrreversibleRollback)])
    );
}

#[test]
fn test_a_lost_writer_blocks_on_quorum() {
    let members = vec![writer(None), replica("b", Some(10))];
    let verdict = rollout_preflight(
        target(2, 2),
        &membership(),
        v(1),
        &members,
        DurabilityPolicy::Majority,
        10,
        budget(0, 0, 0),
    );

    assert_eq!(
        verdict,
        RolloutPreflight::Blocked(vec![Quorum(ReadinessBlocker::WriterLost)])
    );
}

#[test]
fn test_too_few_reporting_members_blocks_on_quorum() {
    let members = vec![writer(Some(20)), replica("b", None)];
    let verdict = rollout_preflight(
        target(2, 2),
        &membership(),
        v(1),
        &members,
        DurabilityPolicy::Majority,
        20,
        budget(0, 100, 100),
    );

    assert_eq!(
        verdict,
        RolloutPreflight::Blocked(vec![Quorum(ReadinessBlocker::InsufficientMembers {
            reporting: 1,
            required: 2,
        })])
    );
}

#[test]
fn test_draining_below_the_serving_budget_blocks_on_capacity() {
    let verdict = rollout_preflight(
        target(2, 2),
        &membership(),
        v(1),
        &caught_up(),
        DurabilityPolicy::Majority,
        10,
        budget(2, 0, 0),
    );

    assert_eq!(
        verdict,
        RolloutPreflight::Blocked(vec![Capacity {
            serving_after_drain: 1,
            required: 2,
        }])
    );
}

#[test]
fn test_a_lagging_group_blocks_on_replication_lag() {
    let members = vec![writer(Some(10)), replica("b", Some(3))];
    let verdict = rollout_preflight(
        target(2, 2),
        &membership(),
        v(1),
        &members,
        DurabilityPolicy::Majority,
        10,
        budget(1, 5, 0),
    );

    assert_eq!(
        verdict,
        RolloutPreflight::Blocked(vec![ReplicationLag { lag: 7, allowed: 5 }])
    );
}

#[test]
fn test_a_stale_backup_blocks_on_backup_lag() {
    let verdict = rollout_preflight(
        target(2, 2),
        &membership(),
        v(1),
        &caught_up(),
        DurabilityPolicy::Majority,
        4,
        budget(1, 0, 3),
    );

    assert_eq!(
        verdict,
        RolloutPreflight::Blocked(vec![BackupLag { lag: 6, allowed: 3 }])
    );
}

#[test]
fn test_lag_at_the_budget_still_clears() {
    let members = vec![writer(Some(10)), replica("b", Some(5))];
    let verdict = rollout_preflight(
        target(2, 2),
        &membership(),
        v(1),
        &members,
        DurabilityPolicy::Majority,
        7,
        budget(1, 5, 3),
    );

    assert_eq!(verdict, RolloutPreflight::Ready);
}

#[test]
fn test_every_failing_rule_surfaces_in_fixed_order() {
    let membership = vec![versions((1, 3), (2, 3))];
    let members = vec![writer(Some(20)), replica("b", None)];
    let verdict = rollout_preflight(
        target(2, 1),
        &membership,
        v(2),
        &members,
        DurabilityPolicy::Majority,
        0,
        budget(2, 5, 5),
    );

    assert_eq!(
        verdict,
        RolloutPreflight::Blocked(vec![
            Version(TargetUnsupported),
            Version(IrreversibleRollback),
            Quorum(ReadinessBlocker::InsufficientMembers {
                reporting: 1,
                required: 2,
            }),
            Capacity {
                serving_after_drain: 0,
                required: 2,
            },
            ReplicationLag { lag: 20, allowed: 5 },
            BackupLag { lag: 20, allowed: 5 },
        ])
    );
}

#[test]
fn test_a_doubled_member_counts_once_toward_capacity() {
    let members = vec![replica("b", Some(5)), replica("b", Some(5)), writer(Some(10))];
    let verdict = rollout_preflight(
        target(2, 2),
        &membership(),
        v(1),
        &members,
        DurabilityPolicy::Majority,
        10,
        budget(2, 5, 0),
    );

    assert_eq!(
        verdict,
        RolloutPreflight::Blocked(vec![Capacity {
            serving_after_drain: 1,
            required: 2,
        }])
    );
}

#[test]
fn test_is_ready_reflects_the_verdict() {
    assert!(RolloutPreflight::Ready.is_ready());
    assert!(
        !RolloutPreflight::Blocked(vec![Capacity {
            serving_after_drain: 0,
            required: 1,
        }])
        .is_ready()
    );
}

#[test]
fn test_upgrade_order_replaces_replicas_before_the_writer() {
    let members = vec![replica("c", Some(1)), writer(Some(1)), replica("a", None)];

    assert_eq!(upgrade_order(&members), vec!["a", "c", "writer"]);
}

#[test]
fn test_upgrade_order_keeps_each_member_once() {
    let members = vec![replica("a", Some(1)), replica("a", Some(1)), writer(Some(1))];

    assert_eq!(upgrade_order(&members), vec!["a", "writer"]);
}

#[test]
fn test_upgrade_order_places_every_writer_last_in_id_order() {
    let members = vec![
        node("w2", MemberRole::Writer, Some(1)),
        node("w1", MemberRole::Writer, Some(1)),
        replica("a", Some(1)),
    ];

    assert_eq!(upgrade_order(&members), vec!["a", "w1", "w2"]);
}
