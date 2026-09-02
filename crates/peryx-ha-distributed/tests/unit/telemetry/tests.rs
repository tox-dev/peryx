use std::time::Duration;

use super::{operation_telemetry, record_blob_ack, record_metadata_ack, sampled};
use crate::support::captured;
use crate::{AuthorityEpoch, BlobReference, Change, MetadataMutation, OperationEnvelope, OperationKind, TraceContext};
use peryx_ha::{
    BlobAckObservation, ByteAckDecision, ByteEvidence, DcAck, DurabilityPolicy, MetadataAckObservation,
    MetadataEvidence, OperationObservation, OperationTrace, WriteAckDecision,
};
use peryx_storage::blob::BlobDurability;

const SAMPLED_TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
const UNSAMPLED_TRACEPARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-00";

fn secret_change() -> Change {
    Change {
        serial: 7,
        event: b"credential=hunter2".to_vec(),
        metadata: vec![MetadataMutation::Put {
            key: "/var/lib/peryx/private/path".to_owned(),
            value: b"secret-digest-map".to_vec(),
        }],
        blobs: vec![BlobReference {
            sha256: "a".repeat(64),
            size: 1024,
        }],
    }
}

fn traced(traceparent: Option<&str>) -> OperationEnvelope {
    OperationEnvelope {
        trace: traceparent.map(|traceparent| TraceContext {
            traceparent: traceparent.to_owned(),
            tracestate: None,
        }),
        ..OperationEnvelope::current("primary-a", AuthorityEpoch(3), OperationKind::Publish, secret_change())
    }
}

fn write_trace(serial: Option<u64>) -> OperationTrace {
    OperationTrace::open(OperationObservation {
        source: "primary-a".to_owned(),
        authority: "repository:alpha".to_owned(),
        epoch: AuthorityEpoch(3),
        serial,
        kind: OperationKind::Publish,
    })
}

fn blob_ack(outcome: DcAck, bytes: &ByteEvidence, metadata_acknowledged: bool) -> BlobAckObservation<'_> {
    BlobAckObservation {
        policy: DurabilityPolicy::Majority,
        outcome,
        bytes,
        metadata_acknowledged,
        bytes_expired: false,
        metadata_expired: false,
        waited: Duration::from_millis(250),
    }
}

#[test]
fn test_a_durable_blob_write_records_the_members_that_acknowledged_it() {
    let bytes = ByteEvidence::Filesystem(ByteAckDecision::Acknowledged {
        nodes: vec!["primary-a".to_owned(), "primary-b".to_owned()],
        required: 2,
    });
    let trace = write_trace(Some(11));

    let recorded = captured(|| {
        record_blob_ack(
            &trace,
            &blob_ack(
                DcAck::Durable {
                    scope: BlobDurability::Filesystem,
                },
                &bytes,
                true,
            ),
        );
    });

    assert!(recorded.contains("availability blob write acknowledged"), "{recorded}");
    for field in [
        trace.traceparent.as_str(),
        "operation.source=\"primary-a\"",
        "operation.authority=\"repository:alpha\"",
        "operation.epoch=3",
        "operation.serial=11",
        "operation.kind=\"publish\"",
        "ack.policy=\"majority\"",
        "ack.outcome=\"durable\"",
        "ack.scope=\"filesystem\"",
        "ack.evidence=\"filesystem\"",
        "ack.nodes=\"primary-a,primary-b\"",
        "ack.required=2",
        "ack.remaining=0",
        "ack.bytes_acknowledged=true",
        "ack.metadata_acknowledged=true",
        "ack.waited_seconds=0.25",
    ] {
        assert!(recorded.contains(field), "missing {field}: {recorded}");
    }
}

/// The verdict alone says a write is not durable. Only the two dimensions say which one it waits on.
#[test]
fn test_an_unproven_blob_write_records_the_dimension_that_expired() {
    let bytes = ByteEvidence::Filesystem(ByteAckDecision::Pending {
        nodes: vec!["primary-a".to_owned()],
        required: 3,
        remaining: 2,
    });
    let trace = write_trace(Some(11));

    let recorded = captured(|| {
        record_blob_ack(
            &trace,
            &BlobAckObservation {
                bytes_expired: true,
                ..blob_ack(DcAck::Unknown, &bytes, false)
            },
        );
    });

    for field in [
        "ack.outcome=\"unknown\"",
        "ack.scope=\"none\"",
        "ack.nodes=\"primary-a\"",
        "ack.required=3",
        "ack.remaining=2",
        "ack.bytes_acknowledged=false",
        "ack.metadata_acknowledged=false",
        "ack.bytes_expired=true",
        "ack.metadata_expired=false",
    ] {
        assert!(recorded.contains(field), "missing {field}: {recorded}");
    }
}

#[test]
fn test_a_pending_blob_write_records_the_metadata_dimension_it_waits_on() {
    let bytes = ByteEvidence::Filesystem(ByteAckDecision::Acknowledged {
        nodes: vec!["primary-a".to_owned()],
        required: 1,
    });

    let recorded = captured(|| {
        record_blob_ack(
            &write_trace(Some(11)),
            &BlobAckObservation {
                metadata_expired: true,
                ..blob_ack(DcAck::Pending, &bytes, false)
            },
        );
    });

    for field in [
        "ack.outcome=\"pending\"",
        "ack.bytes_acknowledged=true",
        "ack.metadata_acknowledged=false",
        "ack.metadata_expired=true",
    ] {
        assert!(recorded.contains(field), "missing {field}: {recorded}");
    }
}

/// An object store publishes the single copy every reader shares, so counting node receipts against it
/// would report the same object once per reader instead of a second copy.
#[test]
fn test_an_object_store_write_records_no_node_quorum() {
    let bytes = ByteEvidence::ObjectStore { acknowledged: true };

    let recorded = captured(|| {
        record_blob_ack(
            &write_trace(Some(11)),
            &blob_ack(
                DcAck::Durable {
                    scope: BlobDurability::ObjectStore,
                },
                &bytes,
                true,
            ),
        );
    });

    for field in [
        "ack.scope=\"object-store\"",
        "ack.evidence=\"object-store\"",
        "ack.nodes=\"\"",
        "ack.required=0",
        "ack.remaining=0",
        "ack.bytes_acknowledged=true",
    ] {
        assert!(recorded.contains(field), "missing {field}: {recorded}");
    }
}

/// Reporting an absent serial as zero would name the journal's first commit, which is a different write.
#[test]
fn test_a_write_that_journaled_nothing_records_no_serial() {
    let bytes = ByteEvidence::ObjectStore { acknowledged: true };

    let recorded = captured(|| {
        record_blob_ack(
            &write_trace(None),
            &blob_ack(
                DcAck::Durable {
                    scope: BlobDurability::ObjectStore,
                },
                &bytes,
                true,
            ),
        );
    });

    assert!(recorded.contains("availability blob write acknowledged"), "{recorded}");
    assert!(!recorded.contains("operation.serial"), "{recorded}");
}

#[test]
fn test_a_metadata_write_records_its_journal_frontier_proof() {
    let trace = write_trace(Some(11));

    let recorded = captured(|| {
        record_metadata_ack(
            &trace,
            MetadataAckObservation {
                policy: DurabilityPolicy::Everywhere,
                evidence: MetadataEvidence::JournalFrontier,
                waited: Duration::from_millis(500),
                timed_out: true,
                decision: WriteAckDecision::Unavailable,
            },
        );
    });

    assert!(
        recorded.contains("availability metadata write acknowledged"),
        "{recorded}"
    );
    for field in [
        trace.traceparent.as_str(),
        "operation.serial=11",
        "ack.policy=\"everywhere\"",
        "ack.outcome=\"unavailable\"",
        "ack.evidence=\"journal-frontier\"",
        "ack.expired=true",
        "ack.waited_seconds=0.5",
    ] {
        assert!(recorded.contains(field), "missing {field}: {recorded}");
    }
}

#[test]
fn test_telemetry_carries_the_identity_kind_and_traceparent() {
    let telemetry = operation_telemetry(&traced(Some(SAMPLED_TRACEPARENT)));

    assert_eq!(telemetry.source, "primary-a");
    assert_eq!(telemetry.epoch, 3);
    assert_eq!(telemetry.serial, 7);
    assert_eq!(telemetry.kind, "publish");
    assert_eq!(telemetry.traceparent.as_deref(), Some(SAMPLED_TRACEPARENT));
    assert!(telemetry.sampled);
}

#[test]
fn test_telemetry_drops_the_change_payload() {
    let rendered = serde_json::to_string(&operation_telemetry(&traced(Some(SAMPLED_TRACEPARENT)))).unwrap();

    for secret in [
        "credential=hunter2",
        "hunter2",
        "secret-digest-map",
        "/var/lib/peryx/private/path",
        &"a".repeat(64),
    ] {
        assert!(
            !rendered.contains(secret),
            "telemetry leaked a payload value: {secret}\n{rendered}"
        );
    }
}

#[test]
fn test_an_untraced_operation_omits_the_traceparent_and_is_not_sampled() {
    let telemetry = operation_telemetry(&traced(None));

    assert_eq!(telemetry.traceparent, None);
    assert!(!telemetry.sampled);
    let rendered = serde_json::to_string(&telemetry).unwrap();
    assert!(
        !rendered.contains("traceparent"),
        "an absent traceparent is omitted: {rendered}"
    );
}

#[test]
fn test_sampled_reads_the_traceparent_flag() {
    assert!(sampled(Some(SAMPLED_TRACEPARENT)));
    assert!(!sampled(Some(UNSAMPLED_TRACEPARENT)));
    assert!(!sampled(None));
}

#[test]
fn test_sampled_rejects_a_malformed_flags_field() {
    assert!(
        !sampled(Some("00-abc-def")),
        "a non-two-digit flags field is not sampled"
    );
    assert!(
        !sampled(Some("00-trace-parent-zz")),
        "a non-hex flags field is not sampled"
    );
    assert!(
        !sampled(Some("nodashes")),
        "a string with no flags field is not sampled"
    );
}

#[test]
fn test_emit_records_a_sampled_operation_without_its_payload() {
    let recorded = captured(|| operation_telemetry(&traced(Some(SAMPLED_TRACEPARENT))).emit());

    assert!(
        recorded.contains("availability operation"),
        "the event is recorded: {recorded}"
    );
    assert!(recorded.contains("primary-a"), "the identity is present: {recorded}");
    assert!(
        recorded.contains(SAMPLED_TRACEPARENT),
        "the traceparent is present: {recorded}"
    );
    for secret in ["hunter2", "secret-digest-map", "/var/lib/peryx/private/path"] {
        assert!(
            !recorded.contains(secret),
            "emit leaked a payload value {secret}: {recorded}"
        );
    }
}

#[test]
fn test_emit_skips_an_unsampled_operation() {
    let recorded = captured(|| operation_telemetry(&traced(Some(UNSAMPLED_TRACEPARENT))).emit());

    assert!(
        recorded.is_empty(),
        "an unsampled operation records nothing: {recorded}"
    );
}
