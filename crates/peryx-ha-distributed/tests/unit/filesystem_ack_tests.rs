use std::collections::BTreeSet;

use peryx_storage::blob::{BlobDurability, Digest};
use rstest::rstest;

use crate::ack::AckDecision;
use crate::dc_ack::Deadline::{Expired, Live};
use crate::dc_ack::{DcAck, Deadline, decide_dc_ack};
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

fn decide(ack: &FilesystemAck, metadata: AckDecision, deadline: Deadline) -> DcAck {
    decide_dc_ack(metadata, &ack.evidence(), deadline)
}

fn awaiting() -> FilesystemAck {
    FilesystemAck::new(digest(1), members(&["a", "b", "c"]), DurabilityPolicy::Majority)
}

#[test]
fn test_two_of_three_members_acknowledge_without_the_third() {
    let mut ack = awaiting();
    assert_eq!(ack.record(receipt("a", digest(1))), ReceiptOutcome::Recorded);
    assert_eq!(ack.record(receipt("b", digest(1))), ReceiptOutcome::Recorded);

    assert_eq!(ack.independent_receipts(), 2);
    assert_eq!(
        decide(&ack, AckDecision::Acknowledged, Live),
        DcAck::Durable {
            scope: BlobDurability::Filesystem
        }
    );
}

#[test]
fn test_a_retry_resumes_from_the_preserved_copy_and_reaches_quorum() {
    let mut ack = awaiting();
    assert_eq!(ack.record(receipt("a", digest(1))), ReceiptOutcome::Recorded);

    assert_eq!(decide(&ack, AckDecision::Acknowledged, Expired), DcAck::Unknown);

    assert_eq!(ack.record(receipt("a", digest(1))), ReceiptOutcome::Ignored);
    assert_eq!(ack.record(receipt("b", digest(1))), ReceiptOutcome::Recorded);
    assert_eq!(
        decide(&ack, AckDecision::Acknowledged, Live),
        DcAck::Durable {
            scope: BlobDurability::Filesystem
        }
    );
}

#[rstest]
#[case::duplicate("a", 1, true, 1)]
#[case::wrong_digest("a", 2, false, 0)]
#[case::nonmember("z", 1, false, 0)]
fn test_invalid_receipts_are_ignored(
    #[case] node: &str,
    #[case] digest_byte: u8,
    #[case] seed: bool,
    #[case] expected_receipts: usize,
) {
    let mut ack = awaiting();
    if seed {
        assert_eq!(ack.record(receipt("a", digest(1))), ReceiptOutcome::Recorded);
    }
    assert_eq!(ack.record(receipt(node, digest(digest_byte))), ReceiptOutcome::Ignored);

    assert_eq!(ack.independent_receipts(), expected_receipts);
}

#[test]
fn test_an_empty_roster_never_acknowledges_durable() {
    let ack = FilesystemAck::new(digest(1), members(&[]), DurabilityPolicy::Everywhere);

    assert_eq!(decide(&ack, AckDecision::Acknowledged, Live), DcAck::Pending);
    assert_eq!(decide(&ack, AckDecision::Acknowledged, Expired), DcAck::Unknown);
}

#[rstest]
#[case::live(Live, DcAck::Pending)]
#[case::expired(Expired, DcAck::Unknown)]
fn test_below_quorum_follows_the_deadline(#[case] deadline: Deadline, #[case] expected: DcAck) {
    let mut ack = awaiting();
    ack.record(receipt("a", digest(1)));

    assert_eq!(decide(&ack, AckDecision::Acknowledged, deadline), expected);
}

#[test]
fn test_metadata_not_yet_acknowledged_holds_the_write_pending_despite_byte_quorum() {
    let mut ack = awaiting();
    ack.record(receipt("a", digest(1)));
    ack.record(receipt("b", digest(1)));

    assert_eq!(
        decide(&ack, AckDecision::NotReady(ReadinessBlocker::WriterLost), Live),
        DcAck::Pending
    );
}
