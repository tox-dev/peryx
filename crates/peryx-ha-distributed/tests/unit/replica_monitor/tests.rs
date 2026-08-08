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
