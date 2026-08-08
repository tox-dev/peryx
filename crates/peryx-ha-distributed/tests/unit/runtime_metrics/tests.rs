use super::*;

const SERIES_BUDGET: usize = 1 + SyncErrorClass::ALL.len() + 1 + LATENCY_BUCKETS_SECONDS.len() + 3;

fn rendered(metrics: &AvailabilityMetrics) -> String {
    let mut body = String::new();
    metrics.write_metrics(&mut body);
    body
}

#[test]
fn test_error_class_classifies_each_bounded_reason() {
    assert_eq!(
        SyncErrorClass::of(&SyncError::UnsupportedVersion { actual: 9, expected: 1 }),
        SyncErrorClass::Schema
    );
    assert_eq!(
        SyncErrorClass::of(&SyncError::primary(std::io::Error::other("primary unreachable"))),
        SyncErrorClass::Transport
    );
    assert_eq!(SyncErrorClass::of(&SyncError::EmptySource), SyncErrorClass::Apply);
}

#[test]
fn test_record_error_counts_only_the_matching_class() {
    let metrics = AvailabilityMetrics::default();
    metrics.record_error(
        &SyncError::UnsupportedVersion { actual: 9, expected: 1 },
        Duration::ZERO,
    );
    metrics.record_error(&SyncError::EmptySource, Duration::ZERO);
    metrics.record_error(&SyncError::EmptySource, Duration::ZERO);

    let body = rendered(&metrics);
    assert!(
        body.contains("peryx_availability_sync_errors_total{class=\"schema\"} 1\n"),
        "{body}"
    );
    assert!(
        body.contains("peryx_availability_sync_errors_total{class=\"transport\"} 0\n"),
        "{body}"
    );
    assert!(
        body.contains("peryx_availability_sync_errors_total{class=\"apply\"} 2\n"),
        "{body}"
    );
    assert!(body.contains("peryx_availability_sync_cycles_total 3\n"), "{body}");
}

#[test]
fn test_record_cycle_reports_queue_depth_from_the_frontier_gap() {
    let metrics = AvailabilityMetrics::default();
    metrics.record_cycle(
        SyncOutcome {
            changes: 2,
            serial: 4,
            primary_serial: 7,
        },
        Duration::from_millis(1),
    );
    assert!(rendered(&metrics).contains("peryx_availability_pending_serials 3\n"));

    metrics.record_cycle(
        SyncOutcome {
            changes: 3,
            serial: 7,
            primary_serial: 7,
        },
        Duration::from_millis(1),
    );
    assert!(rendered(&metrics).contains("peryx_availability_pending_serials 0\n"));
}

#[test]
fn test_histogram_buckets_are_cumulative_across_bounds() {
    let metrics = AvailabilityMetrics::default();
    metrics.record_cycle(
        SyncOutcome {
            changes: 0,
            serial: 1,
            primary_serial: 1,
        },
        Duration::from_millis(1),
    );
    metrics.record_cycle(
        SyncOutcome {
            changes: 0,
            serial: 1,
            primary_serial: 1,
        },
        Duration::from_secs(30),
    );

    let body = rendered(&metrics);
    assert!(
        body.contains("peryx_availability_apply_seconds_bucket{le=\"0.005\"} 1\n"),
        "{body}"
    );
    assert!(
        body.contains("peryx_availability_apply_seconds_bucket{le=\"10\"} 1\n"),
        "{body}"
    );
    assert!(
        body.contains("peryx_availability_apply_seconds_bucket{le=\"+Inf\"} 2\n"),
        "{body}"
    );
    assert!(body.contains("peryx_availability_apply_seconds_count 2\n"), "{body}");
}

#[test]
fn test_exposition_stays_within_the_series_budget() {
    let metrics = AvailabilityMetrics::default();
    metrics.record_error(&SyncError::EmptySource, Duration::from_millis(1));

    let body = rendered(&metrics);
    let series = body
        .lines()
        .filter(|line| line.starts_with("peryx_availability"))
        .count();
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
            "series carries a high-cardinality label: {forbidden}\n{body}"
        );
    }
}
