use std::num::NonZeroUsize;

use crate::readiness::{
    DurabilityPolicy, GroupReadiness, MemberFrontier, MemberRole, ReadinessBlocker, group_readiness,
    visibility_compaction_frontier,
};
use crate::visibility::{ArtifactId, OpOrder, VisibilityAction, VisibilityOp, VisibilityState};

fn artifact(coordinate: &str) -> ArtifactId {
    ArtifactId {
        coordinate: coordinate.to_owned(),
        digest: "sha256:deadbeef".to_owned(),
    }
}

fn visible_at(state: &mut VisibilityState, coordinate: &str, epoch: u64, serial: u64) {
    state.apply(&VisibilityOp {
        artifact: artifact(coordinate),
        action: VisibilityAction::Restore,
        order: OpOrder { epoch, serial },
    });
}

fn writer(applied: Option<u64>) -> MemberFrontier {
    MemberFrontier {
        member: "writer".to_owned(),
        role: MemberRole::Writer,
        applied,
    }
}

fn replica(name: &str, applied: Option<u64>) -> MemberFrontier {
    MemberFrontier {
        member: name.to_owned(),
        role: MemberRole::Replica,
        applied,
    }
}

fn at_least(acks: usize) -> DurabilityPolicy {
    DurabilityPolicy::AtLeast(NonZeroUsize::new(acks).unwrap())
}

#[test]
fn test_majority_stays_ready_and_ignores_a_lagging_replica() {
    let members = [writer(Some(10)), replica("b", Some(10)), replica("c", Some(2))];

    let readiness = group_readiness(&members, DurabilityPolicy::Majority);

    assert!(readiness.is_ready());
    assert_eq!(
        readiness.durable_frontier, 10,
        "the lagging replica must not drag the frontier down"
    );
}

#[test]
fn test_writer_loss_is_not_ready() {
    let members = [writer(None), replica("b", Some(9)), replica("c", Some(9))];

    let readiness = group_readiness(&members, DurabilityPolicy::Majority);

    assert_eq!(readiness.blocked, Some(ReadinessBlocker::WriterLost));
    assert!(!readiness.is_ready());
}

#[test]
fn test_durable_frontier_survives_writer_loss() {
    let members = [writer(None), replica("b", Some(7)), replica("c", Some(7))];

    let readiness = group_readiness(&members, DurabilityPolicy::Majority);

    assert_eq!(readiness.blocked, Some(ReadinessBlocker::WriterLost));
    assert_eq!(readiness.durable_frontier, 7);
}

#[test]
fn test_majority_is_ready_exactly_at_the_quorum_boundary() {
    let members = [writer(Some(5)), replica("b", Some(5)), replica("c", None)];

    let readiness = group_readiness(&members, DurabilityPolicy::Majority);

    assert!(readiness.is_ready(), "two of three reporting meets a majority");
    assert_eq!(readiness.durable_frontier, 5);
}

#[test]
fn test_majority_is_not_ready_one_below_the_quorum_boundary() {
    let members = [writer(Some(5)), replica("b", None), replica("c", None)];

    let readiness = group_readiness(&members, DurabilityPolicy::Majority);

    assert_eq!(
        readiness.blocked,
        Some(ReadinessBlocker::InsufficientMembers {
            reporting: 1,
            required: 2
        }),
    );
    assert_eq!(
        readiness.durable_frontier, 0,
        "only the writer holds a serial, short of the majority"
    );
}

#[test]
fn test_durable_frontier_is_the_minimum_over_the_required_set_not_the_slowest() {
    let members = [writer(Some(10)), replica("b", Some(8)), replica("c", Some(2))];

    let readiness = group_readiness(&members, DurabilityPolicy::Majority);

    assert_eq!(
        readiness.durable_frontier, 8,
        "the two fastest reach 8; the slowest at 2 does not count"
    );
}

#[test]
fn test_local_acknowledges_from_the_writer_alone() {
    let members = [writer(Some(6)), replica("b", None), replica("c", None)];

    let readiness = group_readiness(&members, DurabilityPolicy::Local);

    assert!(readiness.is_ready(), "local durability needs only the writer");
    assert_eq!(readiness.durable_frontier, 6);
}

#[test]
fn test_everywhere_blocks_on_a_single_lost_replica() {
    let members = [writer(Some(4)), replica("b", Some(4)), replica("c", None)];

    let readiness = group_readiness(&members, DurabilityPolicy::Everywhere);

    assert_eq!(
        readiness.blocked,
        Some(ReadinessBlocker::InsufficientMembers {
            reporting: 2,
            required: 3
        }),
    );
    assert_eq!(
        readiness.durable_frontier, 0,
        "a serial every member must hold is not guaranteed yet"
    );
}

#[test]
fn test_everywhere_is_ready_when_all_members_report() {
    let members = [writer(Some(4)), replica("b", Some(5)), replica("c", Some(4))];

    let readiness = group_readiness(&members, DurabilityPolicy::Everywhere);

    assert!(readiness.is_ready());
    assert_eq!(
        readiness.durable_frontier, 4,
        "the slowest member bounds an all-members guarantee"
    );
}

#[test]
fn test_at_least_beyond_the_group_can_never_be_ready() {
    let members = [writer(Some(9)), replica("b", Some(9))];

    let readiness = group_readiness(&members, at_least(3));

    assert_eq!(
        readiness.blocked,
        Some(ReadinessBlocker::InsufficientMembers {
            reporting: 2,
            required: 3
        }),
    );
    assert_eq!(
        readiness.durable_frontier, 0,
        "three acknowledgements are impossible in a two-member group"
    );
}

#[test]
fn test_at_least_two_reports_the_second_highest_frontier() {
    let members = [writer(Some(12)), replica("b", Some(7)), replica("c", Some(7))];

    let readiness = group_readiness(&members, at_least(2));

    assert!(readiness.is_ready());
    assert_eq!(readiness.durable_frontier, 7);
}

#[test]
fn test_empty_group_under_everywhere_is_not_ready() {
    let readiness = group_readiness(&[], DurabilityPolicy::Everywhere);

    assert_eq!(readiness.blocked, Some(ReadinessBlocker::WriterLost));
    assert_eq!(readiness.durable_frontier, 0);
}

#[test]
fn test_required_acks_rounds_a_majority_up_past_half() {
    assert_eq!(DurabilityPolicy::Local.required_acks(5), 1);
    assert_eq!(DurabilityPolicy::Majority.required_acks(3), 2);
    assert_eq!(DurabilityPolicy::Majority.required_acks(4), 3);
    assert_eq!(DurabilityPolicy::Everywhere.required_acks(4), 4);
    assert_eq!(at_least(2).required_acks(5), 2);
}

#[test]
fn test_group_readiness_is_the_negation_of_a_blocker() {
    let ready = GroupReadiness {
        blocked: None,
        durable_frontier: 3,
    };
    let blocked = GroupReadiness {
        blocked: Some(ReadinessBlocker::WriterLost),
        durable_frontier: 0,
    };

    assert!(ready.is_ready());
    assert!(!blocked.is_ready());
}

#[test]
fn test_the_backup_frontier_bounds_the_compaction_frontier() {
    let members = [writer(Some(10)), replica("b", Some(10))];
    let frontier = visibility_compaction_frontier(&members, DurabilityPolicy::Everywhere, 5, 1);

    let mut state = VisibilityState::new();
    visible_at(&mut state, "at-the-backup", 1, 5);
    visible_at(&mut state, "above-the-backup", 1, 8);
    state.compact(&frontier);

    assert_eq!(
        state.retained_artifacts(),
        1,
        "the backup at serial 5 bounds release, so the serial-8 entry stays"
    );
}

#[test]
fn test_the_replicated_frontier_bounds_the_compaction_frontier() {
    let members = [writer(Some(4)), replica("b", Some(4))];
    let frontier = visibility_compaction_frontier(&members, DurabilityPolicy::Everywhere, 100, 1);

    let mut state = VisibilityState::new();
    visible_at(&mut state, "at-the-replicas", 1, 4);
    visible_at(&mut state, "above-the-replicas", 1, 8);
    state.compact(&frontier);

    assert_eq!(
        state.retained_artifacts(),
        1,
        "the replicas at serial 4 bound release despite a further-ahead backup"
    );
}

#[test]
fn test_a_lagging_member_lowers_the_compaction_frontier() {
    let members = [writer(Some(10)), replica("b", Some(3))];
    let frontier = visibility_compaction_frontier(&members, DurabilityPolicy::Everywhere, 100, 1);

    let mut state = VisibilityState::new();
    visible_at(&mut state, "above-the-laggard", 1, 5);
    state.compact(&frontier);

    assert_eq!(
        state.retained_artifacts(),
        1,
        "the lagging replica at serial 3 keeps the serial-5 entry retained"
    );
}

#[test]
fn test_the_frontier_acknowledges_the_given_epoch() {
    let members = [writer(Some(10)), replica("b", Some(10))];
    let frontier = visibility_compaction_frontier(&members, DurabilityPolicy::Everywhere, 10, 7);

    let mut state = VisibilityState::new();
    visible_at(&mut state, "this-epoch", 7, 5);
    visible_at(&mut state, "later-epoch", 9, 1);
    state.compact(&frontier);

    assert_eq!(
        state.retained_artifacts(),
        1,
        "the frontier covers epoch 7, so only the later-epoch entry stays"
    );
}

#[test]
fn test_the_frontier_drains_every_epoch_below_the_current_authority() {
    let members = [writer(Some(10)), replica("b", Some(10))];
    let frontier = visibility_compaction_frontier(&members, DurabilityPolicy::Everywhere, 10, 5);

    let mut state = VisibilityState::new();
    visible_at(&mut state, "superseded-epoch", 2, 999);
    state.compact(&frontier);

    assert_eq!(
        state.retained_artifacts(),
        0,
        "an entry left in an epoch below the current authority is drained and released"
    );
}

#[test]
fn test_a_duplicate_member_id_never_inflates_the_quorum() {
    let honest = group_readiness(&[writer(Some(5)), replica("a", None)], DurabilityPolicy::Majority);
    assert!(!honest.is_ready());

    let doubled = group_readiness(
        &[writer(Some(5)), writer(Some(5)), replica("a", None)],
        DurabilityPolicy::Majority,
    );
    assert!(!doubled.is_ready(), "a doubled member id forged a majority");
}

#[test]
fn test_the_durable_frontier_never_exceeds_the_writer() {
    let readiness = group_readiness(&[writer(Some(5)), replica("a", Some(9))], DurabilityPolicy::Local);
    assert_eq!(
        readiness.durable_frontier, 5,
        "a replica above the writer over-claimed durability"
    );
}

#[test]
fn test_a_filtered_roster_computes_a_smaller_quorum_than_the_true_group() {
    let full = [writer(Some(5)), replica("a", None), replica("b", None)];
    assert!(!group_readiness(&full, DurabilityPolicy::Majority).is_ready());

    // Filtering silent members would shrink the quorum and report false readiness.
    assert!(group_readiness(&[writer(Some(5))], DurabilityPolicy::Majority).is_ready());
}
