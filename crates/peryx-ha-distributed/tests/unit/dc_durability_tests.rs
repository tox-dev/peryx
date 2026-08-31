use super::*;
use crate::byte_ack::decide_byte_ack;
use crate::readiness::DurabilityPolicy;
use crate::receipt_quorum::ReceiptAck;
use peryx_storage::blob::Digest;

const SERIES_BUDGET: usize = SCOPES.len() + 2 + 3;

fn rendered(metrics: &DcDurabilityMetrics) -> String {
    let mut body = String::new();
    metrics.write_metrics(&mut body);
    body
}

#[test]
fn test_record_counts_a_durable_write_under_its_backend_scope() {
    let metrics = DcDurabilityMetrics::default();
    metrics.record(DcAck::Durable {
        scope: BlobDurability::Filesystem,
    });
    metrics.record(DcAck::Durable {
        scope: BlobDurability::ObjectStore,
    });
    metrics.record(DcAck::Durable {
        scope: BlobDurability::ObjectStore,
    });

    let body = rendered(&metrics);
    assert!(
        body.contains("peryx_dc_ack_durable_total{scope=\"filesystem\"} 1\n"),
        "{body}"
    );
    assert!(
        body.contains("peryx_dc_ack_durable_total{scope=\"object-store\"} 2\n"),
        "{body}"
    );
}

#[test]
fn test_record_counts_pending_and_unknown_outcomes_apart() {
    let metrics = DcDurabilityMetrics::default();
    metrics.record(DcAck::Pending);
    metrics.record(DcAck::Unknown);
    metrics.record(DcAck::Unknown);

    let body = rendered(&metrics);
    assert!(body.contains("peryx_dc_ack_pending_total 1\n"), "{body}");
    assert!(body.contains("peryx_dc_ack_unknown_total 2\n"), "{body}");
    assert!(
        body.contains("peryx_dc_ack_durable_total{scope=\"filesystem\"} 0\n"),
        "an unexercised scope still reports zero: {body}"
    );
}

#[test]
fn test_record_quorum_reports_progress_from_a_pending_decision() {
    let metrics = DcDurabilityMetrics::default();
    metrics.record_quorum(&byte_decision(&["east", "east", "outside"]));

    let body = rendered(&metrics);
    assert!(body.contains("peryx_dc_ack_quorum_acknowledged 1\n"), "{body}");
    assert!(body.contains("peryx_dc_ack_quorum_required 2\n"), "{body}");
    assert!(body.contains("peryx_dc_ack_quorum_remaining 1\n"), "{body}");
}

#[test]
fn test_record_quorum_reports_a_complete_decision_with_nothing_remaining() {
    let metrics = DcDurabilityMetrics::default();
    metrics.record_quorum(&byte_decision(&["east", "west", "south"]));

    let body = rendered(&metrics);
    assert!(body.contains("peryx_dc_ack_quorum_acknowledged 3\n"), "{body}");
    assert!(body.contains("peryx_dc_ack_quorum_required 2\n"), "{body}");
    assert!(body.contains("peryx_dc_ack_quorum_remaining 0\n"), "{body}");
}

#[test]
fn test_exposition_stays_within_the_series_budget() {
    let metrics = DcDurabilityMetrics::default();
    metrics.record(DcAck::Durable {
        scope: BlobDurability::Filesystem,
    });
    metrics.record(DcAck::Pending);
    metrics.record(DcAck::Unknown);
    metrics.record_quorum(&ByteAckDecision::Pending {
        nodes: vec!["east".to_owned()],
        required: 3,
        remaining: 2,
    });

    let body = rendered(&metrics);
    let series = body.lines().filter(|line| line.starts_with("peryx_dc_ack")).count();
    assert_eq!(series, SERIES_BUDGET);
    for forbidden in [
        "tenant",
        "resource",
        "group",
        "artifact",
        "repository",
        "digest",
        "operation",
        "trace",
        "node",
    ] {
        assert!(
            !body.contains(forbidden),
            "a series carries a high-cardinality label: {forbidden}\n{body}"
        );
    }
}

#[test]
fn test_write_ack_observer_records_outcome_and_quorum() {
    let metrics = DcDurabilityMetrics::default();
    peryx_ha::WriteAckObserver::record(
        &metrics,
        DcAck::Pending,
        &ByteEvidence::Filesystem(ByteAckDecision::Pending {
            nodes: vec!["east".to_owned()],
            required: 2,
            remaining: 1,
        }),
    );

    let body = rendered(&metrics);
    assert!(body.contains("peryx_dc_ack_pending_total 1\n"), "{body}");
    assert!(body.contains("peryx_dc_ack_quorum_required 2\n"), "{body}");
}

/// An object store answers for the bytes itself, so no node quorum ran and the gauges must not claim one.
#[test]
fn test_an_object_store_write_reports_its_scope_without_a_node_quorum() {
    let metrics = DcDurabilityMetrics::default();
    peryx_ha::WriteAckObserver::record(
        &metrics,
        DcAck::Durable {
            scope: BlobDurability::ObjectStore,
        },
        &ByteEvidence::ObjectStore { acknowledged: true },
    );

    let body = rendered(&metrics);
    assert!(
        body.contains("peryx_dc_ack_durable_total{scope=\"object-store\"} 1\n"),
        "{body}"
    );
    assert!(body.contains("peryx_dc_ack_quorum_required 0\n"), "{body}");
}

fn byte_decision(receipt_nodes: &[&str]) -> ByteAckDecision {
    let digest = Digest::of(b"artifact");
    decide_byte_ack(
        &digest,
        &receipt_nodes
            .iter()
            .map(|node| ReceiptAck {
                node: (*node).to_owned(),
                digest: digest.clone(),
            })
            .collect::<Vec<_>>(),
        &["east", "west", "south"].map(str::to_owned).into_iter().collect(),
        DurabilityPolicy::Majority,
    )
}
