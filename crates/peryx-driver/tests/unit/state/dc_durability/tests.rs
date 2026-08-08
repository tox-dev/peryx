use super::*;

/// The number of `peryx_dc_ack_*` series the recorder renders: one durable counter per scope, the
/// pending and unknown counters, and the three quorum gauges. A constant of the metric shape, not the
/// load, held by [`test_exposition_stays_within_the_series_budget`].
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
    metrics.record_quorum(&ByteAckDecision::Pending {
        nodes: vec!["east".to_owned(), "west".to_owned()],
        remaining: 1,
    });

    let body = rendered(&metrics);
    assert!(body.contains("peryx_dc_ack_quorum_acknowledged 2\n"), "{body}");
    assert!(body.contains("peryx_dc_ack_quorum_required 3\n"), "{body}");
    assert!(body.contains("peryx_dc_ack_quorum_remaining 1\n"), "{body}");
}

#[test]
fn test_record_quorum_reports_a_complete_decision_with_nothing_remaining() {
    let metrics = DcDurabilityMetrics::default();
    metrics.record_quorum(&ByteAckDecision::Acknowledged {
        nodes: vec!["east".to_owned(), "west".to_owned(), "south".to_owned()],
    });

    let body = rendered(&metrics);
    assert!(body.contains("peryx_dc_ack_quorum_acknowledged 3\n"), "{body}");
    assert!(body.contains("peryx_dc_ack_quorum_required 3\n"), "{body}");
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
        remaining: 2,
    });

    let body = rendered(&metrics);
    let series = body.lines().filter(|line| line.starts_with("peryx_dc_ack")).count();
    assert_eq!(series, SERIES_BUDGET);
    for forbidden in [
        "tenant",
        "project",
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
