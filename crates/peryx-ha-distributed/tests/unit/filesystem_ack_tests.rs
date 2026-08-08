use std::collections::BTreeSet;

use peryx_storage::blob::{BlobDurability, Digest};

use crate::ack::AckDecision;
use crate::dc_ack::DcAck;
use crate::dc_ack::Deadline::{Expired, Live};
use crate::filesystem_ack::{FilesystemAck, ReceiptOutcome};
use crate::readiness::{DurabilityPolicy, ReadinessBlocker};
use crate::receipt_quorum::ReceiptAck;

fn digest(byte: u8) -> Digest {
    Digest::of(&[byte])
}

fn receipt(node: &str, digest: Digest) -> ReceiptAck {
    ReceiptAck {
        node: node.to_owned(),
        digest,
    }
}

fn members(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// A three-member group whose majority quorum is two.
fn awaiting() -> FilesystemAck {
    FilesystemAck::new(digest(1), members(&["a", "b", "c"]), DurabilityPolicy::Majority)
}

#[test]
fn test_two_of_three_members_acknowledge_without_the_third() {
    // Three configured members with a majority quorum of two; node "c" never returns a receipt, yet
    // the two that do meet quorum, so a lost node does not fail the write.
    let mut ack = awaiting();
    assert_eq!(ack.record(receipt("a", digest(1))), ReceiptOutcome::Recorded);
    assert_eq!(ack.record(receipt("b", digest(1))), ReceiptOutcome::Recorded);

    assert_eq!(ack.independent_receipts(), 2);
    assert_eq!(
        ack.decide(AckDecision::Acknowledged, Live),
        DcAck::Durable {
            scope: BlobDurability::Filesystem
        }
    );
}

#[test]
fn test_a_retry_resumes_from_the_preserved_copy_and_reaches_quorum() {
    let mut ack = awaiting();
    assert_eq!(ack.record(receipt("a", digest(1))), ReceiptOutcome::Recorded);

    // The client deadline expires with one copy: retry-safe (unknown), never a definite failure.
    assert_eq!(ack.decide(AckDecision::Acknowledged, Expired), DcAck::Unknown);

    // The retry keeps the completed copy, so a re-delivered receipt from the same node is a no-op and
    // only the remaining node's receipt is needed to acknowledge.
    assert_eq!(ack.record(receipt("a", digest(1))), ReceiptOutcome::Ignored);
    assert_eq!(ack.record(receipt("b", digest(1))), ReceiptOutcome::Recorded);
    assert_eq!(
        ack.decide(AckDecision::Acknowledged, Live),
        DcAck::Durable {
            scope: BlobDurability::Filesystem
        }
    );
}

#[test]
fn test_a_second_receipt_from_one_node_is_ignored() {
    let mut ack = awaiting();
    assert_eq!(ack.record(receipt("a", digest(1))), ReceiptOutcome::Recorded);
    assert_eq!(ack.record(receipt("a", digest(1))), ReceiptOutcome::Ignored);

    assert_eq!(
        ack.independent_receipts(),
        1,
        "one node counts once however many it returns"
    );
}

#[test]
fn test_a_receipt_for_another_digest_is_ignored() {
    let mut ack = awaiting();
    assert_eq!(ack.record(receipt("a", digest(2))), ReceiptOutcome::Ignored);

    assert_eq!(ack.independent_receipts(), 0);
}

#[test]
fn test_a_receipt_from_a_nonmember_is_ignored() {
    let mut ack = awaiting();
    // "z" is not one of the configured members, so its receipt cannot join the quorum.
    assert_eq!(ack.record(receipt("z", digest(1))), ReceiptOutcome::Ignored);

    assert_eq!(ack.independent_receipts(), 0);
}

#[test]
fn test_an_empty_roster_never_acknowledges_durable() {
    // Everywhere over zero members must not report a write durable with no receipt: the floored quorum
    // keeps it pending even when the metadata dimension is acknowledged.
    let ack = FilesystemAck::new(digest(1), members(&[]), DurabilityPolicy::Everywhere);

    assert_eq!(ack.decide(AckDecision::Acknowledged, Live), DcAck::Pending);
    assert_eq!(ack.decide(AckDecision::Acknowledged, Expired), DcAck::Unknown);
}

#[test]
fn test_below_quorum_within_the_deadline_is_pending() {
    let mut ack = awaiting();
    ack.record(receipt("a", digest(1)));

    assert_eq!(ack.decide(AckDecision::Acknowledged, Live), DcAck::Pending);
}

#[test]
fn test_a_deadline_that_expires_below_quorum_is_unknown() {
    let mut ack = awaiting();
    ack.record(receipt("a", digest(1)));

    assert_eq!(
        ack.decide(AckDecision::Acknowledged, Expired),
        DcAck::Unknown,
        "a completed copy may have landed after the client stopped waiting"
    );
}

#[test]
fn test_metadata_not_yet_acknowledged_holds_the_write_pending_despite_byte_quorum() {
    let mut ack = awaiting();
    ack.record(receipt("a", digest(1)));
    ack.record(receipt("b", digest(1)));

    assert_eq!(
        ack.decide(AckDecision::NotReady(ReadinessBlocker::WriterLost), Live),
        DcAck::Pending
    );
}
