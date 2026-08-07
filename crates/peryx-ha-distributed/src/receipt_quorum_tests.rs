use std::collections::BTreeSet;

use peryx_storage::blob::Digest;

use crate::readiness::DurabilityPolicy;
use crate::receipt_quorum::{ByteDurability, ReceiptAck, assess_byte_durability};

fn ack(node: &str, digest: &Digest) -> ReceiptAck {
    ReceiptAck {
        node: node.to_owned(),
        digest: digest.clone(),
    }
}

fn nodes(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn members(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

#[test]
fn test_majority_acknowledges_after_two_of_three_independent_nodes() {
    let digest = Digest::of(b"artifact");

    let outcome = assess_byte_durability(
        &digest,
        &[ack("a", &digest), ack("b", &digest)],
        &members(&["a", "b", "c"]),
        DurabilityPolicy::Majority,
    );

    assert_eq!(
        outcome,
        ByteDurability::Durable {
            nodes: nodes(&["a", "b"])
        }
    );
    assert!(outcome.is_durable());
}

#[test]
fn test_majority_is_pending_with_one_of_three() {
    let digest = Digest::of(b"artifact");

    let outcome = assess_byte_durability(
        &digest,
        &[ack("a", &digest)],
        &members(&["a", "b", "c"]),
        DurabilityPolicy::Majority,
    );

    assert_eq!(
        outcome,
        ByteDurability::Pending {
            nodes: nodes(&["a"]),
            required: 2,
        }
    );
    assert!(!outcome.is_durable());
    assert_eq!(outcome.nodes(), nodes(&["a"]).as_slice());
}

#[test]
fn test_two_paths_on_one_node_count_once() {
    let digest = Digest::of(b"artifact");

    // One node returning twice is not two independent copies.
    let outcome = assess_byte_durability(
        &digest,
        &[ack("a", &digest), ack("a", &digest)],
        &members(&["a", "b", "c"]),
        DurabilityPolicy::Majority,
    );

    assert_eq!(
        outcome,
        ByteDurability::Pending {
            nodes: nodes(&["a"]),
            required: 2,
        }
    );
}

#[test]
fn test_a_receipt_for_another_digest_never_counts() {
    let digest = Digest::of(b"artifact");
    let other = Digest::of(b"other");

    let outcome = assess_byte_durability(
        &digest,
        &[ack("a", &digest), ack("b", &other)],
        &members(&["a", "b", "c"]),
        DurabilityPolicy::Majority,
    );

    assert_eq!(
        outcome,
        ByteDurability::Pending {
            nodes: nodes(&["a"]),
            required: 2,
        }
    );
}

#[test]
fn test_a_receipt_from_a_nonmember_never_counts() {
    let digest = Digest::of(b"artifact");

    // A node outside the configured group cannot contribute to the quorum, however genuine its receipt.
    let outcome = assess_byte_durability(
        &digest,
        &[ack("z", &digest), ack("a", &digest)],
        &members(&["a", "b", "c"]),
        DurabilityPolicy::Majority,
    );

    assert_eq!(
        outcome,
        ByteDurability::Pending {
            nodes: nodes(&["a"]),
            required: 2,
        }
    );
}

#[test]
fn test_node_loss_still_acknowledges_when_the_rest_meet_quorum() {
    let digest = Digest::of(b"artifact");

    // Three configured, node "c" lost; "a" and "b" alone meet the majority, and the evidence is the
    // distinct nodes in stable order regardless of arrival order.
    let outcome = assess_byte_durability(
        &digest,
        &[ack("b", &digest), ack("a", &digest)],
        &members(&["a", "b", "c"]),
        DurabilityPolicy::Majority,
    );

    assert!(outcome.is_durable());
    assert_eq!(outcome.nodes(), nodes(&["a", "b"]).as_slice());
}

#[test]
fn test_local_policy_acknowledges_from_a_single_node() {
    let digest = Digest::of(b"artifact");

    let outcome = assess_byte_durability(
        &digest,
        &[ack("a", &digest)],
        &members(&["a", "b", "c"]),
        DurabilityPolicy::Local,
    );

    assert!(outcome.is_durable());
}

#[test]
fn test_everywhere_policy_needs_every_configured_node() {
    let digest = Digest::of(b"artifact");

    let outcome = assess_byte_durability(
        &digest,
        &[ack("a", &digest), ack("b", &digest)],
        &members(&["a", "b", "c"]),
        DurabilityPolicy::Everywhere,
    );

    assert_eq!(
        outcome,
        ByteDurability::Pending {
            nodes: nodes(&["a", "b"]),
            required: 3,
        }
    );
}

#[test]
fn test_an_empty_roster_is_pending_not_vacuously_durable() {
    let digest = Digest::of(b"artifact");

    // Everywhere over zero members would compute a required quorum of zero; the floor keeps the write
    // pending on at least one node rather than acknowledging it durable with no receipt at all.
    let outcome = assess_byte_durability(&digest, &[], &members(&[]), DurabilityPolicy::Everywhere);

    assert_eq!(
        outcome,
        ByteDurability::Pending {
            nodes: Vec::new(),
            required: 1,
        }
    );
    assert!(!outcome.is_durable());
}

#[test]
fn test_no_receipts_are_pending() {
    let digest = Digest::of(b"artifact");

    let outcome = assess_byte_durability(&digest, &[], &members(&["a", "b", "c"]), DurabilityPolicy::Majority);

    assert_eq!(
        outcome,
        ByteDurability::Pending {
            nodes: Vec::new(),
            required: 2,
        }
    );
}
