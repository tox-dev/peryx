use super::*;

#[test]
fn blob_metrics_accumulate_fetches_and_replace_pending() {
    let monitor = ReplicaMonitor::new(0);
    monitor.record_blobs(BlobPlaneReport { fetched: 2, pending: 3 });
    monitor.record_blobs(BlobPlaneReport { fetched: 1, pending: 0 });

    let mut body = String::new();
    monitor.write_metrics(&mut body);
    assert!(body.contains("peryx_ha_distributed_blobs_fetched_total 3\n"), "{body}");
    assert!(body.contains("peryx_ha_distributed_blobs_pending 0\n"), "{body}");
}

#[test]
fn metadata_progress_updates_the_snapshot_and_readiness() {
    let monitor = ReplicaMonitor::new(2);
    monitor.record(SyncOutcome {
        changes: 3,
        serial: 5,
        primary_serial: 7,
    });
    monitor.record_readable(4);

    let observation = monitor.snapshot();
    assert_eq!(observation.serial, 5);
    assert_eq!(observation.primary_serial, Some(7));
    assert_eq!(observation.changes, 3);
    assert_eq!(observation.readable_serial, 4);
    assert_eq!(monitor.readiness_gap(), Some("frontier_lag"));

    monitor.record(SyncOutcome {
        changes: 0,
        serial: 7,
        primary_serial: 7,
    });
    assert_eq!(monitor.readiness_gap(), None);

    let mut body = String::new();
    monitor.write_metrics(&mut body);
    assert!(body.contains("peryx_ha_distributed_primary_serial 7\n"), "{body}");
}

#[test]
fn errors_replace_the_readiness_fault_and_accumulate() {
    let monitor = ReplicaMonitor::new(0);
    monitor.record_error(&SyncError::EmptySource);
    assert_eq!(monitor.readiness_gap(), Some("sync_error"));

    monitor.record_error(&SyncError::UnsupportedVersion { actual: 2, expected: 1 });
    assert_eq!(monitor.readiness_gap(), Some("incompatible_schema"));
    assert_eq!(monitor.snapshot().errors, 2);
}
