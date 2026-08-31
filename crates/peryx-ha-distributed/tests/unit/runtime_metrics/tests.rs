use super::*;
use crate::SyncOutcome;
use crate::replica_cycle::RetiredPeers;

fn applied(serial: u64, primary_serial: u64, blobs: BlobPass, elapsed: Duration) -> ReplicaCycle {
    ReplicaCycle {
        metadata: Ok(SyncOutcome {
            changes: 2,
            serial,
            primary_serial,
        }),
        blobs,
        readable: serial,
        retired: None,
        elapsed,
    }
}

fn failed(error: SyncError, elapsed: Duration) -> ReplicaCycle {
    ReplicaCycle {
        metadata: Err(error),
        blobs: BlobPass::Skipped,
        readable: 0,
        retired: Some(RetiredPeers {
            peers: Vec::new(),
            fully_retired: false,
        }),
        elapsed,
    }
}

fn complete() -> BlobPass {
    BlobPass::Completed(crate::BlobPlaneReport { fetched: 0, pending: 0 })
}

const SERIES_BUDGET: usize =
    1 + SyncErrorClass::ALL.len() + HeartbeatErrorClass::ALL.len() + 1 + LATENCY_BUCKETS_SECONDS.len() + 3;

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
fn test_a_failed_cycle_counts_only_the_matching_class() {
    let metrics = AvailabilityMetrics::default();
    metrics.record_cycle(&failed(
        SyncError::UnsupportedVersion { actual: 9, expected: 1 },
        Duration::ZERO,
    ));
    metrics.record_cycle(&failed(SyncError::EmptySource, Duration::ZERO));
    metrics.record_cycle(&failed(SyncError::EmptySource, Duration::ZERO));

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
    metrics.record_cycle(&applied(4, 7, complete(), Duration::from_millis(1)));
    assert!(rendered(&metrics).contains("peryx_availability_pending_serials 3\n"));

    metrics.record_cycle(&applied(7, 7, complete(), Duration::from_millis(1)));
    assert!(rendered(&metrics).contains("peryx_availability_pending_serials 0\n"));

    // A cycle that applied no page leaves the depth the last applied cycle reported.
    metrics.record_cycle(&failed(SyncError::EmptySource, Duration::from_millis(1)));
    assert!(rendered(&metrics).contains("peryx_availability_pending_serials 0\n"));
}

#[test]
fn test_histogram_buckets_are_cumulative_across_bounds() {
    let metrics = AvailabilityMetrics::default();
    metrics.record_cycle(&applied(1, 1, complete(), Duration::from_millis(1)));
    metrics.record_cycle(&applied(1, 1, complete(), Duration::from_secs(30)));

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
    metrics.record_cycle(&failed(SyncError::EmptySource, Duration::from_millis(1)));

    let body = rendered(&metrics);
    let series = body
        .lines()
        .filter(|line| line.starts_with("peryx_availability"))
        .count();
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
            "series carries a high-cardinality label: {forbidden}\n{body}"
        );
    }
}

#[test]
fn test_a_cycle_that_loses_one_plane_is_still_one_cycle() {
    let metrics = AvailabilityMetrics::default();

    metrics.record_cycle(&applied(
        4,
        4,
        BlobPass::Failed(SyncError::BlobFetchFailed {
            reason: "chunk_digest_mismatch",
            digest: "abc".to_owned(),
        }),
        Duration::from_millis(1),
    ));

    let body = rendered(&metrics);
    assert!(body.contains("peryx_availability_sync_cycles_total 1\n"), "{body}");
    assert!(
        body.contains("peryx_availability_sync_errors_total{class=\"apply\"} 1\n"),
        "{body}"
    );
    assert!(body.contains("peryx_availability_apply_seconds_count 1\n"), "{body}");
}
