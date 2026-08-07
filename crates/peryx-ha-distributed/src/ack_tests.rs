use crate::ack::{AckDecision, acknowledge};
use crate::readiness::{
    DurabilityPolicy, GroupReadiness, MemberFrontier, MemberRole, ReadinessBlocker, group_readiness,
};

fn ready(durable_frontier: u64) -> GroupReadiness {
    GroupReadiness {
        blocked: None,
        durable_frontier,
    }
}

fn blocked(blocker: ReadinessBlocker, durable_frontier: u64) -> GroupReadiness {
    GroupReadiness {
        blocked: Some(blocker),
        durable_frontier,
    }
}

#[test]
fn test_a_ready_group_acknowledges_up_to_and_including_its_frontier() {
    for (target, expected) in [
        (5, AckDecision::Acknowledged),
        (10, AckDecision::Acknowledged),
        (
            11,
            AckDecision::NotYetDurable {
                target: 11,
                durable_frontier: 10,
            },
        ),
    ] {
        assert_eq!(
            acknowledge(&ready(10), target),
            expected,
            "target {target} against frontier 10"
        );
    }
}

#[test]
fn test_is_acknowledged_reflects_the_decision() {
    assert!(acknowledge(&ready(10), 10).is_acknowledged());
    assert!(!acknowledge(&ready(10), 11).is_acknowledged());
}

#[test]
fn test_a_blocked_group_never_acknowledges_and_carries_its_blocker() {
    for blocker in [
        ReadinessBlocker::WriterLost,
        ReadinessBlocker::InsufficientMembers {
            reporting: 1,
            required: 2,
        },
    ] {
        assert_eq!(
            acknowledge(&blocked(blocker, 0), 1),
            AckDecision::NotReady(blocker),
            "{blocker:?}"
        );
    }
}

#[test]
fn test_a_blocker_outranks_a_write_below_the_durable_frontier() {
    // A serial two replicas already hold sits below the durable frontier, yet a lost writer still blocks
    // acknowledgement: the blocker is decided before the frontier comparison.
    let decision = acknowledge(&blocked(ReadinessBlocker::WriterLost, 7), 3);

    assert_eq!(decision, AckDecision::NotReady(ReadinessBlocker::WriterLost));
}

#[test]
fn test_acknowledge_composes_with_group_readiness() {
    let members = [
        MemberFrontier {
            member: "writer".to_owned(),
            role: MemberRole::Writer,
            applied: Some(9),
        },
        MemberFrontier {
            member: "b".to_owned(),
            role: MemberRole::Replica,
            applied: Some(9),
        },
        MemberFrontier {
            member: "c".to_owned(),
            role: MemberRole::Replica,
            applied: Some(4),
        },
    ];
    let evidence = group_readiness(&members, DurabilityPolicy::Majority);

    assert_eq!(
        acknowledge(&evidence, 9),
        AckDecision::Acknowledged,
        "the majority reaches serial 9"
    );
    assert_eq!(
        acknowledge(&evidence, 10),
        AckDecision::NotYetDurable {
            target: 10,
            durable_frontier: 9,
        },
    );
}
