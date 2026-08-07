use crate::remote_durability::{MetadataOperation, RemoteAck, RemoteDurability, assess_remote_metadata_durability};

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

#[test]
fn test_one_eligible_remote_makes_the_write_durable() {
    let outcome = assess_remote_metadata_durability(&op(3, 100), &[ack("east", 3, 100)]);

    assert_eq!(
        outcome,
        RemoteDurability::Durable {
            holders: holders(&["east"])
        }
    );
    assert!(outcome.is_durable());
    assert!(outcome.is_sole_copy());
}

#[test]
fn test_no_acknowledgements_are_pending() {
    let outcome = assess_remote_metadata_durability(&op(3, 100), &[]);

    assert_eq!(outcome, RemoteDurability::Pending);
    assert!(!outcome.is_durable());
    assert!(outcome.holders().is_empty());
}

#[test]
fn test_a_remote_behind_the_frontier_is_not_eligible() {
    // Applied 99, but the operation needs 100.
    let outcome = assess_remote_metadata_durability(&op(3, 100), &[ack("east", 3, 99)]);

    assert_eq!(outcome, RemoteDurability::Pending);
}

#[test]
fn test_a_remote_at_a_stale_epoch_is_fenced() {
    let outcome = assess_remote_metadata_durability(&op(3, 100), &[ack("east", 2, 100)]);

    assert_eq!(outcome, RemoteDurability::Pending);
}

#[test]
fn test_a_remote_at_an_advanced_epoch_is_fenced() {
    let outcome = assess_remote_metadata_durability(&op(3, 100), &[ack("east", 4, 100)]);

    assert_eq!(outcome, RemoteDurability::Pending);
}

#[test]
fn test_a_remote_past_the_frontier_is_eligible() {
    // Applied 150 covers an operation at 100.
    let outcome = assess_remote_metadata_durability(&op(3, 100), &[ack("east", 3, 150)]);

    assert!(outcome.is_durable());
}

#[test]
fn test_every_eligible_remote_is_named_in_stable_order() {
    let outcome = assess_remote_metadata_durability(
        &op(3, 100),
        &[ack("west", 3, 100), ack("east", 3, 120), ack("south", 2, 100)],
    );

    // west and east are eligible, south is fenced by epoch; holders are distinct and sorted.
    assert_eq!(
        outcome,
        RemoteDurability::Durable {
            holders: holders(&["east", "west"])
        }
    );
    assert!(!outcome.is_sole_copy());
}

#[test]
fn test_a_datacenter_acknowledging_twice_counts_once() {
    let outcome = assess_remote_metadata_durability(&op(3, 100), &[ack("east", 3, 100), ack("east", 3, 130)]);

    assert_eq!(
        outcome,
        RemoteDurability::Durable {
            holders: holders(&["east"])
        }
    );
    assert!(outcome.is_sole_copy());
}
