use std::str::FromStr as _;

use peryx_ha::{
    ArtifactSource, BackendId, BackendLocation, BlobPlacementFailure, BlobPlacementKey, BlobPlacementOutcome,
    BlobPlacementState, BlobPlacementTransition, DataCenterId, HomePlacementRecorder as _, MAX_PLACEMENTS_PER_DIGEST,
    PlacementEvent,
};
use peryx_identity::ArtifactDigest;
use peryx_storage::meta::MetaStore;

use crate::placement_policy::DistributedHomePlacementRecorder;
use crate::{
    BlobPlacementError, apply_blob_placement, apply_placement_event, record_artifact_placement, record_local_placement,
    route_blob_placements,
};

fn digest(suffix: u8) -> ArtifactDigest {
    ArtifactDigest::from_str(&format!("sha256:{suffix:064x}")).unwrap()
}

fn key(data_center: &str, location: &str) -> BlobPlacementKey {
    BlobPlacementKey {
        digest: digest(1),
        backend: BackendId::new("filesystem").unwrap(),
        data_center: DataCenterId::new(data_center).unwrap(),
        location: BackendLocation::new(location).unwrap(),
    }
}

fn store() -> (tempfile::TempDir, MetaStore) {
    let directory = tempfile::tempdir().unwrap();
    let store = MetaStore::open(directory.path().join("peryx.redb")).unwrap();
    (directory, store)
}

fn verify(store: &MetaStore, key: &BlobPlacementKey) {
    apply_blob_placement(store, key, &BlobPlacementTransition::Stage, 3, 10).unwrap();
    apply_blob_placement(
        store,
        key,
        &BlobPlacementTransition::Verify {
            attempt: 1,
            observed: key.digest.clone(),
            size: 1,
        },
        3,
        20,
    )
    .unwrap();
}

#[test]
fn test_routing_partitions_every_placement_state() {
    let (_directory, store) = store();
    verify(&store, &key("home", "local"));
    verify(&store, &key("away", "remote"));
    apply_blob_placement(&store, &key("home", "pending"), &BlobPlacementTransition::Stage, 3, 10).unwrap();
    let failed = key("home", "failed");
    apply_blob_placement(&store, &failed, &BlobPlacementTransition::Stage, 3, 10).unwrap();
    apply_blob_placement(
        &store,
        &failed,
        &BlobPlacementTransition::Fail {
            attempt: 1,
            class: BlobPlacementFailure::BackendRejected,
        },
        3,
        20,
    )
    .unwrap();
    let revoked = key("home", "revoked");
    apply_blob_placement(&store, &revoked, &BlobPlacementTransition::Stage, 3, 10).unwrap();
    apply_blob_placement(&store, &revoked, &BlobPlacementTransition::Revoke, 3, 20).unwrap();

    let routing = route_blob_placements(
        store.blob_placements(&digest(1)).unwrap(),
        &DataCenterId::new("home").unwrap(),
    );

    assert_eq!(routing.local.len(), 1);
    assert_eq!(routing.verified_remote.len(), 1);
    assert_eq!(routing.pending.len(), 1);
    assert_eq!(routing.failed.len(), 1);
    assert_eq!(routing.revoked.len(), 1);
    assert!(routing.is_serveable());
    assert!(!routing.is_empty());
}

#[test]
fn test_routing_without_a_verified_copy_is_not_serveable() {
    let (_directory, store) = store();
    let empty = route_blob_placements(Vec::new(), &DataCenterId::new("home").unwrap());
    assert!(empty.is_empty());
    assert!(!empty.is_serveable());
    apply_blob_placement(&store, &key("home", "pending"), &BlobPlacementTransition::Stage, 3, 10).unwrap();

    let routing = route_blob_placements(
        store.blob_placements(&digest(1)).unwrap(),
        &DataCenterId::new("home").unwrap(),
    );

    assert!(!routing.is_empty());
    assert!(!routing.is_serveable());
}

#[test]
fn test_record_local_placement_verifies_the_home_copy() {
    let (_directory, store) = store();
    let backend = BackendId::new("filesystem").unwrap();

    let outcome = record_local_placement(
        &store,
        &backend,
        &DataCenterId::new("home").unwrap(),
        &digest(7),
        2_048,
        3,
        100,
    )
    .unwrap();

    assert_eq!(outcome.record().state, BlobPlacementState::Verified { size: 2_048 });
    let routing = route_blob_placements(
        store.blob_placements(&digest(7)).unwrap(),
        &DataCenterId::new("peer").unwrap(),
    );
    assert_eq!(routing.verified_remote.len(), 1);
    assert_eq!(
        routing.verified_remote[0].key.data_center,
        DataCenterId::new("home").unwrap()
    );
}

#[test]
fn test_home_placement_recorder_validates_and_records_the_copy() {
    let (_directory, store) = store();
    let recorder = DistributedHomePlacementRecorder::new(
        store.clone(),
        BackendId::new("filesystem").unwrap(),
        DataCenterId::new("home").unwrap(),
        std::sync::Arc::new(|| 100),
    );

    assert!(recorder.record("invalid", 2_048, 3).is_err());
    recorder.record(digest(7).sha256(), 2_048, 3).unwrap();

    assert_eq!(
        store.blob_placements(&digest(7)).unwrap()[0].state,
        BlobPlacementState::Verified { size: 2_048 }
    );
}

#[test]
fn test_home_placement_recorder_surfaces_storage_failures() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("peryx.redb");
    let store = MetaStore::open(&path).unwrap();
    let recorder = DistributedHomePlacementRecorder::new(
        store.clone(),
        BackendId::new("filesystem").unwrap(),
        DataCenterId::new("home").unwrap(),
        std::sync::Arc::new(|| 100),
    );
    recorder.record(digest(7).sha256(), 2_048, 3).unwrap();
    drop(recorder);
    drop(store);
    let database = redb::Database::open(&path).unwrap();
    let transaction = database.begin_write().unwrap();
    {
        use redb::ReadableTable as _;
        let mut table = transaction
            .open_table(redb::TableDefinition::<&str, &[u8]>::new("blob_placement"))
            .unwrap();
        let key = table.first().unwrap().unwrap().0.value().to_owned();
        table.insert(key.as_str(), b"invalid".as_slice()).unwrap();
    }
    transaction.commit().unwrap();
    drop(database);
    let recorder = DistributedHomePlacementRecorder::new(
        MetaStore::open_existing(path).unwrap(),
        BackendId::new("filesystem").unwrap(),
        DataCenterId::new("home").unwrap(),
        std::sync::Arc::new(|| 200),
    );

    assert!(recorder.record(digest(7).sha256(), 2_048, 4).is_err());
}

#[test]
fn test_record_local_placement_is_idempotent() {
    let (_directory, store) = store();
    let backend = BackendId::new("filesystem").unwrap();
    let data_center = DataCenterId::new("home").unwrap();
    record_local_placement(&store, &backend, &data_center, &digest(7), 2_048, 3, 100).unwrap();

    let outcome = record_local_placement(&store, &backend, &data_center, &digest(7), 2_048, 3, 200).unwrap();

    assert!(matches!(outcome, peryx_ha::BlobPlacementOutcome::Unchanged(_)));
}

#[test]
fn test_record_local_placement_recovers_a_failed_copy() {
    let (_directory, store) = store();
    let backend = BackendId::new("filesystem").unwrap();
    let data_center = DataCenterId::new("home").unwrap();
    let key = BlobPlacementKey {
        digest: digest(7),
        backend: backend.clone(),
        data_center: data_center.clone(),
        location: BackendLocation::for_digest(&digest(7)),
    };
    apply_blob_placement(&store, &key, &BlobPlacementTransition::Stage, 3, 10).unwrap();
    apply_blob_placement(
        &store,
        &key,
        &BlobPlacementTransition::Fail {
            attempt: 1,
            class: BlobPlacementFailure::DigestMismatch,
        },
        3,
        20,
    )
    .unwrap();

    let outcome = record_local_placement(&store, &backend, &data_center, &digest(7), 2_048, 3, 100).unwrap();

    assert_eq!(outcome.record().state, BlobPlacementState::Verified { size: 2_048 });
}

#[test]
fn test_record_local_placement_restores_a_revoked_copy() {
    let (_directory, store) = store();
    let backend = BackendId::new("filesystem").unwrap();
    let data_center = DataCenterId::new("home").unwrap();
    let artifact = digest(7);
    let key = BlobPlacementKey {
        digest: artifact.clone(),
        backend: backend.clone(),
        data_center: data_center.clone(),
        location: BackendLocation::for_digest(&artifact),
    };
    record_local_placement(&store, &backend, &data_center, &artifact, 2_048, 3, 10).unwrap();
    apply_blob_placement(&store, &key, &BlobPlacementTransition::Revoke, 4, 20).unwrap();

    let outcome = record_local_placement(&store, &backend, &data_center, &artifact, 2_048, 4, 30).unwrap();

    assert_eq!(
        outcome,
        BlobPlacementOutcome::Applied(peryx_ha::BlobPlacementRecord {
            key,
            state: BlobPlacementState::Verified { size: 2_048 },
            fence: 4,
            transfer_attempt: 0,
            generation: 3,
            updated_at_unix: 30,
        })
    );
}

#[test]
fn test_record_local_placement_completes_an_interrupted_stage() {
    let (_directory, store) = store();
    let backend = BackendId::new("filesystem").unwrap();
    let data_center = DataCenterId::new("home").unwrap();
    let artifact = digest(7);
    let key = BlobPlacementKey {
        digest: artifact.clone(),
        backend: backend.clone(),
        data_center: data_center.clone(),
        location: BackendLocation::for_digest(&artifact),
    };
    apply_blob_placement(&store, &key, &BlobPlacementTransition::Stage, 3, 10).unwrap();

    let outcome = record_local_placement(&store, &backend, &data_center, &artifact, 2_048, 3, 30).unwrap();

    assert_eq!(
        outcome,
        BlobPlacementOutcome::Applied(peryx_ha::BlobPlacementRecord {
            key,
            state: BlobPlacementState::Verified { size: 2_048 },
            fence: 3,
            transfer_attempt: 1,
            generation: 2,
            updated_at_unix: 30,
        })
    );
}

#[test]
fn test_record_local_placement_claims_a_newer_fence() {
    let (_directory, store) = store();
    let backend = BackendId::new("filesystem").unwrap();
    let data_center = DataCenterId::new("home").unwrap();
    record_local_placement(&store, &backend, &data_center, &digest(7), 2_048, 3, 100).unwrap();

    let outcome = record_local_placement(&store, &backend, &data_center, &digest(7), 2_048, 9, 200).unwrap();

    assert_eq!(outcome.record().fence, 9);
    assert_eq!(store.blob_placements(&digest(7)).unwrap()[0].fence, 9);
}

#[test]
fn test_concurrent_local_placements_settle_one_verified_row() {
    let (_directory, store) = store();
    let backend = BackendId::new("filesystem").unwrap();
    let data_center = DataCenterId::new("home").unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
    let settled = std::thread::scope(|scope| {
        let writes: [_; 16] = std::array::from_fn(|_| {
            let barrier = barrier.clone();
            let backend = backend.clone();
            let data_center = data_center.clone();
            let store = store.clone();
            scope.spawn(move || {
                barrier.wait();
                record_local_placement(&store, &backend, &data_center, &digest(7), 2_048, 3, 100)
            })
        });
        let outcomes: Vec<_> = writes.into_iter().map(|write| write.join().unwrap()).collect();
        let applied = outcomes
            .iter()
            .filter(|outcome| matches!(outcome, Ok(BlobPlacementOutcome::Applied(_))))
            .count();
        (applied, outcomes.iter().filter(|outcome| outcome.is_ok()).count())
    });

    assert_eq!(settled, (1, 16));
    assert_eq!(
        store.blob_placements(&digest(7)).unwrap(),
        vec![peryx_ha::BlobPlacementRecord {
            key: BlobPlacementKey {
                digest: digest(7),
                backend,
                data_center,
                location: BackendLocation::for_digest(&digest(7)),
            },
            state: BlobPlacementState::Verified { size: 2_048 },
            fence: 3,
            transfer_attempt: 0,
            generation: 1,
            updated_at_unix: 100,
        }]
    );
}

#[test]
fn test_record_local_placement_rejects_a_stale_fence() {
    let (_directory, store) = store();
    let backend = BackendId::new("filesystem").unwrap();
    let data_center = DataCenterId::new("home").unwrap();
    let key = BlobPlacementKey {
        digest: digest(7),
        backend: backend.clone(),
        data_center: data_center.clone(),
        location: BackendLocation::for_digest(&digest(7)),
    };
    apply_blob_placement(&store, &key, &BlobPlacementTransition::Stage, 5, 10).unwrap();

    let error = record_local_placement(&store, &backend, &data_center, &digest(7), 2_048, 2, 200).unwrap_err();

    assert!(matches!(error, BlobPlacementError::Decision(_)));
}

#[test]
fn test_policy_maps_the_persistence_capacity_bound() {
    let (_directory, store) = store();
    for index in 0..MAX_PLACEMENTS_PER_DIGEST {
        apply_blob_placement(
            &store,
            &key("home", &format!("copy-{index}")),
            &BlobPlacementTransition::Stage,
            3,
            10,
        )
        .unwrap();
    }

    assert!(matches!(
        apply_blob_placement(&store, &key("home", "overflow"), &BlobPlacementTransition::Stage, 3, 10,),
        Err(BlobPlacementError::TooManyPlacements)
    ));
}

#[test]
fn test_apply_placement_returns_the_unchanged_decision() {
    let (_directory, store) = store();
    let key = key("home", "pending");
    apply_blob_placement(&store, &key, &BlobPlacementTransition::Stage, 3, 10).unwrap();

    assert!(matches!(
        apply_blob_placement(&store, &key, &BlobPlacementTransition::Stage, 3, 11).unwrap(),
        BlobPlacementOutcome::Unchanged(_)
    ));
}

#[test]
fn test_placement_event_handles_missing_and_unchanged_records() {
    let (_directory, store) = store();
    assert_eq!(
        apply_placement_event(&store, "missing", PlacementEvent::WriteFailed).unwrap(),
        None
    );
    let original = record_artifact_placement(&store, "present", ArtifactSource::Hosted, true).unwrap();

    assert_eq!(
        apply_placement_event(&store, "present", PlacementEvent::WriteFailed).unwrap(),
        Some(original)
    );
}

#[test]
fn test_concurrent_placement_writes_converge() {
    let (_directory, store) = store();
    for round in 0..32 {
        let key = key("home", &format!("race-{round}"));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
        std::thread::scope(|scope| {
            let writes: [_; 16] = std::array::from_fn(|_| {
                let barrier = barrier.clone();
                let key = key.clone();
                let store = store.clone();
                scope.spawn(move || {
                    barrier.wait();
                    apply_blob_placement(&store, &key, &BlobPlacementTransition::Stage, 3, 10).unwrap()
                })
            });
            assert_eq!(
                writes
                    .into_iter()
                    .map(|write| write.join().unwrap())
                    .filter(|outcome| matches!(outcome, BlobPlacementOutcome::Applied(_)))
                    .count(),
                1
            );
        });
    }
}

#[test]
fn test_concurrent_placement_events_converge() {
    let (_directory, store) = store();
    record_artifact_placement(&store, "contended", ArtifactSource::Hosted, true).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
    std::thread::scope(|scope| {
        let updates = (0..16)
            .map(|index| {
                let barrier = barrier.clone();
                let store = store.clone();
                scope.spawn(move || {
                    for _ in 0..32 {
                        barrier.wait();
                        apply_placement_event(
                            &store,
                            "contended",
                            if index % 2 == 0 {
                                PlacementEvent::BytesVerified
                            } else {
                                PlacementEvent::Repaired { present: false }
                            },
                        )
                        .unwrap();
                    }
                })
            })
            .collect::<Vec<_>>();
        for update in updates {
            update.join().unwrap();
        }
    });

    assert!(store.get_artifact_placement("contended").unwrap().is_some());
}
