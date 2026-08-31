use std::num::NonZeroUsize;

use crate::remote_durability::{
    DurabilityPolicy, MetadataOperation, RemoteAck, RemoteDurability, assess_remote_metadata_durability,
};

fn ack(datacenter: &str, epoch: u64, applied_frontier: u64) -> RemoteAck {
    RemoteAck {
        datacenter: datacenter.to_owned(),
        epoch,
        applied_frontier,
    }
}

fn op(epoch: u64, frontier: u64) -> MetadataOperation {
    MetadataOperation { epoch, frontier }
}

fn holders(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn assess(acks: &[RemoteAck], configured: usize, policy: DurabilityPolicy) -> RemoteDurability {
    assess_remote_metadata_durability(&op(3, 100), acks, configured, policy)
}

#[test]
fn test_one_eligible_remote_makes_a_local_policy_write_durable() {
    let outcome = assess(&[ack("east", 3, 100)], 1, DurabilityPolicy::Local);

    assert_eq!(
        outcome,
        RemoteDurability::Durable {
            holders: holders(&["east"])
        }
    );
    assert!(outcome.is_durable());
}

#[test]
fn test_no_acknowledgements_are_pending() {
    let outcome = assess(&[], 1, DurabilityPolicy::Local);

    assert_eq!(
        outcome,
        RemoteDurability::Pending {
            holders: Vec::new(),
            durable_frontier: 0,
        }
    );
    assert!(!outcome.is_durable());
    assert!(outcome.holders().is_empty());
}

#[test]
fn test_a_remote_behind_the_frontier_is_not_eligible() {
    let outcome = assess(&[ack("east", 3, 99)], 1, DurabilityPolicy::Local);

    assert_eq!(
        outcome,
        RemoteDurability::Pending {
            holders: Vec::new(),
            durable_frontier: 99,
        }
    );
}

#[rstest::rstest]
#[case::stale(2)]
#[case::advanced(4)]
fn test_a_remote_at_another_epoch_is_fenced(#[case] epoch: u64) {
    let outcome = assess(&[ack("east", epoch, 100)], 1, DurabilityPolicy::Local);

    assert_eq!(
        outcome,
        RemoteDurability::Pending {
            holders: Vec::new(),
            durable_frontier: 0,
        },
        "another epoch's serials say nothing about this epoch's frontier"
    );
}

#[test]
fn test_a_remote_past_the_frontier_is_eligible() {
    assert!(assess(&[ack("east", 3, 150)], 1, DurabilityPolicy::Local).is_durable());
}

#[test]
fn test_every_eligible_remote_is_named_in_stable_order() {
    let acks = [ack("west", 3, 100), ack("east", 3, 120), ack("south", 2, 100)];

    let outcome = assess(&acks, 3, DurabilityPolicy::Local);

    assert!(outcome.is_durable());
    assert_eq!(outcome.holders(), holders(&["east", "west"]));
}

#[test]
fn test_a_datacenter_acknowledging_twice_counts_once() {
    let outcome = assess(
        &[ack("east", 3, 100), ack("east", 3, 130)],
        2,
        DurabilityPolicy::Everywhere,
    );

    assert_eq!(
        outcome,
        RemoteDurability::Pending {
            holders: holders(&["east"]),
            durable_frontier: 0,
        },
        "one datacenter reporting twice is not two datacenters"
    );
}

#[test]
fn test_everywhere_requires_every_configured_remote() {
    let acks = [ack("east", 3, 100), ack("west", 3, 100)];

    assert_eq!(
        assess(&acks, 3, DurabilityPolicy::Everywhere),
        RemoteDurability::Pending {
            holders: holders(&["east", "west"]),
            durable_frontier: 0,
        }
    );
    assert!(assess(&acks, 2, DurabilityPolicy::Everywhere).is_durable());
}

#[test]
fn test_majority_requires_over_half_the_configured_remotes() {
    let acks = [ack("east", 3, 100), ack("west", 3, 100)];

    assert!(assess(&acks, 3, DurabilityPolicy::Majority).is_durable());
    assert!(!assess(&acks[..1], 3, DurabilityPolicy::Majority).is_durable());
}

#[test]
fn test_at_least_requires_the_named_datacenter_count() {
    let policy = DurabilityPolicy::AtLeast(NonZeroUsize::new(3).unwrap());
    let acks = [ack("east", 3, 100), ack("west", 3, 100), ack("south", 3, 100)];

    assert!(!assess(&acks[..2], 3, policy).is_durable());
    assert!(assess(&acks, 3, policy).is_durable());
}

#[test]
fn test_an_empty_remote_set_never_claims_durability_from_no_evidence() {
    assert!(
        !assess(&[], 0, DurabilityPolicy::Everywhere).is_durable(),
        "everywhere over zero remotes must not resolve to a zero quorum"
    );
}

#[test]
fn test_a_pending_quorum_reports_the_frontier_the_required_remotes_share() {
    let acks = [ack("east", 3, 90), ack("west", 3, 70), ack("south", 3, 40)];

    assert_eq!(
        assess(&acks, 3, DurabilityPolicy::Majority),
        RemoteDurability::Pending {
            holders: Vec::new(),
            durable_frontier: 70,
        }
    );
    assert_eq!(
        assess(&acks, 3, DurabilityPolicy::Everywhere),
        RemoteDurability::Pending {
            holders: Vec::new(),
            durable_frontier: 40,
        }
    );
}

#[test]
fn test_a_silent_remote_holds_the_quorum_frontier_at_zero() {
    let outcome = assess(&[ack("east", 3, 90)], 3, DurabilityPolicy::Majority);

    assert_eq!(
        outcome,
        RemoteDurability::Pending {
            holders: Vec::new(),
            durable_frontier: 0,
        },
        "a datacenter that never reported has applied nothing"
    );
}

#[test]
fn test_a_datacenters_furthest_report_sets_its_frontier() {
    let outcome = assess(&[ack("east", 3, 40), ack("east", 3, 90)], 2, DurabilityPolicy::Local);

    assert_eq!(
        outcome,
        RemoteDurability::Pending {
            holders: Vec::new(),
            durable_frontier: 90,
        }
    );
}
