use std::time::Duration;

use peryx_storage::meta::MetaError;

use super::{ReplicaMonitor, ReplicaObservation};
use crate::replica_cycle::{BlobPass, ReplicaCycle, RetiredPeers};
use crate::{BlobPlaneReport, RetiredPeer, SyncError, SyncOutcome};
use peryx_core::PrometheusSource as _;

fn applied(serial: u64, primary_serial: u64, readable: u64, blobs: BlobPass) -> ReplicaCycle {
    ReplicaCycle {
        metadata: Ok(SyncOutcome {
            changes: 1,
            serial,
            primary_serial,
        }),
        blobs,
        readable,
        retired: None,
        elapsed: Duration::ZERO,
    }
}

fn complete(fetched: usize, pending: usize) -> BlobPass {
    BlobPass::Completed(BlobPlaneReport { fetched, pending })
}

fn failed(error: SyncError, readable: u64) -> ReplicaCycle {
    ReplicaCycle {
        metadata: Err(error),
        blobs: BlobPass::Skipped,
        readable,
        retired: None,
        elapsed: Duration::ZERO,
    }
}

fn decode_error() -> SyncError {
    SyncError::Store(MetaError::Decode(
        serde_json::from_slice::<u64>(b"\"not a serial\"").unwrap_err(),
    ))
}

fn metrics(monitor: &ReplicaMonitor) -> String {
    let mut body = String::new();
    monitor.write_metrics(&mut body);
    body
}

#[test]
fn test_a_cycle_publishes_every_field_it_carries() {
    let monitor = ReplicaMonitor::new(2);

    monitor.publish(ReplicaCycle {
        metadata: Ok(SyncOutcome {
            changes: 3,
            serial: 5,
            primary_serial: 7,
        }),
        blobs: complete(2, 1),
        readable: 4,
        retired: Some(RetiredPeers {
            peers: vec![RetiredPeer {
                source: "primary".to_owned(),
                reason: "bad_status",
            }],
            fully_retired: false,
        }),
        elapsed: Duration::ZERO,
    });

    let observation = monitor.snapshot();
    assert_eq!(observation.serial, 5);
    assert_eq!(observation.primary_serial, Some(7));
    assert_eq!(observation.changes, 3);
    assert_eq!(observation.readable_serial, 4);
    assert_eq!(observation.errors, 0);
    assert_eq!(
        observation.retired,
        vec![RetiredPeer {
            source: "primary".to_owned(),
            reason: "bad_status",
        }]
    );
    assert!(!observation.fully_retired);
}

#[test]
fn test_a_starting_replica_lags_the_frontier_and_reports_no_primary_serial() {
    let monitor = ReplicaMonitor::new(0);

    assert_eq!(monitor.snapshot().readiness_gaps(), vec!["frontier_lag"]);
    let body = metrics(&monitor);
    assert!(!body.contains("peryx_ha_distributed_primary_serial"), "{body}");
    assert!(body.contains("peryx_ha_distributed_caught_up 0\n"), "{body}");
}

#[test]
fn test_metadata_faults_name_distinct_readiness_reasons() {
    for (error, reason) in [
        (decode_error(), "metadata_store"),
        (SyncError::EmptySource, "sync_error"),
        (
            SyncError::UnsupportedVersion { actual: 2, expected: 1 },
            "incompatible_schema",
        ),
    ] {
        let monitor = ReplicaMonitor::new(0);

        monitor.publish(failed(error, 0));

        let observation = monitor.snapshot();
        assert_eq!(observation.readiness_gaps(), vec![reason, "frontier_lag"]);
        assert_eq!(observation.errors, 1);
    }
}

#[test]
fn test_a_metadata_failure_leaves_the_last_applied_serial_standing() {
    let monitor = ReplicaMonitor::new(0);
    monitor.publish(applied(9, 9, 9, complete(0, 0)));

    monitor.publish(failed(SyncError::EmptySource, 9));

    let observation = monitor.snapshot();
    assert_eq!(observation.serial, 9);
    assert_eq!(observation.primary_serial, Some(9));
    assert_eq!(observation.readiness_gaps(), vec!["sync_error"]);
}

#[test]
fn test_a_metadata_outcome_leaves_the_blob_fault_standing() {
    let monitor = ReplicaMonitor::new(0);

    monitor.publish(applied(
        100,
        100,
        99,
        BlobPass::Failed(SyncError::BlobFetchFailed {
            reason: "chunk_digest_mismatch",
            digest: "abc".to_owned(),
        }),
    ));

    let observation = monitor.snapshot();
    assert_eq!(observation.readiness_gaps(), vec!["blob_plane", "readable_lag"]);
    assert_eq!(observation.errors, 1);
    assert!(metrics(&monitor).contains("peryx_ha_distributed_caught_up 0\n"));
}

#[test]
fn test_a_complete_blob_pass_clears_the_blob_fault_and_restores_readiness() {
    let monitor = ReplicaMonitor::new(0);
    monitor.publish(applied(100, 100, 99, BlobPass::Failed(SyncError::EmptySource)));

    monitor.publish(applied(100, 100, 100, complete(1, 0)));

    let observation = monitor.snapshot();
    assert_eq!(observation.readiness_gaps(), Vec::<&str>::new());
    assert!(observation.is_ready());
    assert!(metrics(&monitor).contains("peryx_ha_distributed_caught_up 1\n"));
}

#[test]
fn test_backpressured_blobs_hold_readiness_until_the_blob_view_advances() {
    let monitor = ReplicaMonitor::new(0);

    monitor.publish(applied(100, 100, 99, complete(0, 3)));

    let backpressured = monitor.snapshot();
    assert_eq!(backpressured.readiness_gaps(), vec!["readable_lag"]);
    assert_eq!(backpressured.errors, 0);

    monitor.publish(applied(100, 100, 100, complete(3, 0)));

    assert!(monitor.snapshot().is_ready());
}

#[test]
fn test_content_deferred_to_a_peer_does_not_block_readiness() {
    let monitor = ReplicaMonitor::new(0);

    monitor.publish(applied(100, 100, 100, complete(0, 2)));

    assert!(monitor.snapshot().is_ready());
    let body = metrics(&monitor);
    assert!(body.contains("peryx_ha_distributed_blobs_pending 2\n"), "{body}");
}

#[test]
fn test_metadata_lag_outranks_the_derived_view_it_bounds() {
    let monitor = ReplicaMonitor::new(0);

    monitor.publish(applied(5, 7, 4, complete(0, 0)));

    assert_eq!(monitor.snapshot().readiness_gaps(), vec!["frontier_lag"]);
}

#[test]
fn test_a_fully_retired_peer_set_gaps_readiness() {
    let monitor = ReplicaMonitor::new(0);

    monitor.publish(ReplicaCycle {
        retired: Some(RetiredPeers {
            peers: Vec::new(),
            fully_retired: true,
        }),
        ..applied(4, 4, 4, complete(0, 0))
    });

    assert_eq!(monitor.snapshot().readiness_gaps(), vec!["retired_peers"]);
}

#[test]
fn test_a_cycle_that_never_reached_the_peers_keeps_the_previous_retirement() {
    let monitor = ReplicaMonitor::new(0);
    monitor.publish(ReplicaCycle {
        retired: Some(RetiredPeers {
            peers: Vec::new(),
            fully_retired: true,
        }),
        ..applied(4, 4, 4, complete(0, 0))
    });

    monitor.publish(failed(SyncError::EmptySource, 4));

    assert!(monitor.snapshot().fully_retired);
}

#[test]
fn test_blob_metrics_accumulate_fetches_and_replace_pending() {
    let monitor = ReplicaMonitor::new(0);

    monitor.publish(applied(1, 1, 1, complete(2, 3)));
    monitor.publish(applied(2, 2, 2, complete(1, 0)));

    let body = metrics(&monitor);
    assert!(body.contains("peryx_ha_distributed_blobs_fetched_total 3\n"), "{body}");
    assert!(body.contains("peryx_ha_distributed_blobs_pending 0\n"), "{body}");
    assert!(body.contains("peryx_ha_distributed_changes_total 2\n"), "{body}");
    assert!(body.contains("peryx_ha_distributed_readable_serial 2\n"), "{body}");
    assert!(body.contains("peryx_ha_distributed_primary_serial 2\n"), "{body}");
    assert!(body.contains("peryx_ha_distributed_lag 0\n"), "{body}");
}

#[test]
fn test_a_snapshot_keeps_the_cycle_it_was_taken_from() {
    let monitor = ReplicaMonitor::new(0);
    monitor.publish(applied(3, 4, 3, complete(0, 0)));

    let first: ReplicaObservation = monitor.snapshot();
    monitor.publish(applied(4, 4, 4, complete(0, 0)));

    assert_eq!(first.serial, 3);
    assert_eq!(first.readiness_gaps(), vec!["frontier_lag"]);
    assert!(monitor.snapshot().is_ready());
}
