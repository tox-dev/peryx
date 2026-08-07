use peryx_storage::blob::BlobDurability;

use crate::ack::AckDecision;
use crate::byte_ack::ByteAckDecision;
use crate::dc_ack::Deadline::{Expired, Live};
use crate::dc_ack::{ByteEvidence, DcAck, decide_dc_ack};
use crate::readiness::ReadinessBlocker;

fn filesystem_durable() -> ByteEvidence {
    ByteEvidence::Filesystem(ByteAckDecision::Acknowledged {
        nodes: vec!["a".to_owned(), "b".to_owned()],
    })
}

fn filesystem_pending() -> ByteEvidence {
    ByteEvidence::Filesystem(ByteAckDecision::Pending {
        nodes: vec!["a".to_owned()],
        remaining: 1,
    })
}

#[test]
fn test_filesystem_durable_metadata_and_bytes_acknowledge_at_filesystem_scope() {
    let decision = decide_dc_ack(AckDecision::Acknowledged, &filesystem_durable(), Live);
    assert_eq!(
        decision,
        DcAck::Durable {
            scope: BlobDurability::Filesystem
        }
    );
}

#[test]
fn test_object_store_durable_metadata_and_bytes_acknowledge_at_object_store_scope() {
    let decision = decide_dc_ack(
        AckDecision::Acknowledged,
        &ByteEvidence::ObjectStore { acknowledged: true },
        Live,
    );
    assert_eq!(
        decision,
        DcAck::Durable {
            scope: BlobDurability::ObjectStore
        }
    );
}

#[test]
fn test_pending_bytes_within_the_deadline_keep_waiting() {
    let decision = decide_dc_ack(AckDecision::Acknowledged, &filesystem_pending(), Live);
    assert_eq!(decision, DcAck::Pending);
}

#[test]
fn test_unacknowledged_object_bytes_within_the_deadline_keep_waiting() {
    let decision = decide_dc_ack(
        AckDecision::Acknowledged,
        &ByteEvidence::ObjectStore { acknowledged: false },
        Live,
    );
    assert_eq!(decision, DcAck::Pending);
}

#[test]
fn test_unready_metadata_within_the_deadline_keeps_waiting() {
    let decision = decide_dc_ack(
        AckDecision::NotReady(ReadinessBlocker::WriterLost),
        &filesystem_durable(),
        Live,
    );
    assert_eq!(decision, DcAck::Pending);
}

#[test]
fn test_metadata_not_yet_durable_within_the_deadline_keeps_waiting() {
    let decision = decide_dc_ack(
        AckDecision::NotYetDurable {
            target: 9,
            durable_frontier: 4,
        },
        &filesystem_durable(),
        Live,
    );
    assert_eq!(decision, DcAck::Pending);
}

#[test]
fn test_an_expired_deadline_is_unknown_not_a_failure() {
    let decision = decide_dc_ack(
        AckDecision::NotReady(ReadinessBlocker::WriterLost),
        &filesystem_durable(),
        Expired,
    );
    assert_eq!(
        decision,
        DcAck::Unknown,
        "durable completion may have occurred after the client stopped waiting"
    );
}
