use crate::meta::{ArtifactPlacementHealth, ArtifactSource};

use super::store;

#[test]
fn test_artifact_placement_health_buckets_by_availability() {
    let (_dir, store) = store();
    assert_eq!(
        store.artifact_placement_health().unwrap(),
        ArtifactPlacementHealth::default()
    );

    store
        .record_artifact_placement("sha256:local", ArtifactSource::Hosted, true)
        .unwrap();
    store
        .record_artifact_placement("sha256:remote", ArtifactSource::Proxy, false)
        .unwrap();
    store
        .record_artifact_placement("sha256:gone", ArtifactSource::Hosted, false)
        .unwrap();
    store
        .record_artifact_placement("sha256:generated", ArtifactSource::Generated, false)
        .unwrap();

    let health = store.artifact_placement_health().unwrap();
    assert_eq!(health.local, 1);
    assert_eq!(health.remote_only, 1);
    assert_eq!(
        health.unavailable, 2,
        "hosted and generated without bytes cannot be served"
    );
    assert_eq!(health.total(), 4);
}

#[test]
fn test_count_artifact_placements_reports_the_recorded_rows() {
    let (_dir, store) = store();
    assert_eq!(store.count_artifact_placements().unwrap(), 0);

    store
        .record_artifact_placement("sha256:aa", ArtifactSource::Hosted, true)
        .unwrap();
    store
        .record_artifact_placement("sha256:bb", ArtifactSource::Proxy, false)
        .unwrap();
    assert_eq!(store.count_artifact_placements().unwrap(), 2);

    store
        .record_artifact_placement("sha256:aa", ArtifactSource::Hosted, false)
        .unwrap();
    assert_eq!(store.count_artifact_placements().unwrap(), 2);

    store.delete_artifact_placement("sha256:aa").unwrap();
    assert_eq!(store.count_artifact_placements().unwrap(), 1);
}
