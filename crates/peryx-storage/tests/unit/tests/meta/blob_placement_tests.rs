use std::str::FromStr as _;

use peryx_identity::ArtifactDigest;
use rstest::rstest;

use crate::meta::{
    BackendId, BackendLocation, BlobPlacementError, BlobPlacementFailure, BlobPlacementKey, BlobPlacementOutcome,
    BlobPlacementState, BlobPlacementStatus, BlobPlacementTransition, DataCenterId, MAX_PLACEMENTS_PER_DIGEST,
    MetaStore, PlacementKeyError,
};

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, store)
}

fn digest(suffix: u8) -> ArtifactDigest {
    ArtifactDigest::from_str(&format!("sha256:{suffix:064x}")).unwrap()
}

fn key(suffix: u8, data_center: &str, location: &str) -> BlobPlacementKey {
    BlobPlacementKey {
        digest: digest(suffix),
        backend: BackendId::new("filesystem").unwrap(),
        data_center: DataCenterId::new(data_center).unwrap(),
        location: BackendLocation::new(location).unwrap(),
    }
}

fn dc(name: &str) -> DataCenterId {
    DataCenterId::new(name).unwrap()
}

#[test]
fn test_key_components_accept_a_valid_value_and_expose_it() {
    assert_eq!(BackendId::new("s3").unwrap().as_str(), "s3");
    assert_eq!(DataCenterId::new("dc-1").unwrap().as_str(), "dc-1");
    assert_eq!(
        BackendLocation::new("blobs/sha256/aa").unwrap().as_str(),
        "blobs/sha256/aa"
    );
}

#[test]
fn test_backend_location_for_digest_carries_the_sha256_hex() {
    let digest = digest(0x2a);
    assert_eq!(BackendLocation::for_digest(&digest).as_str(), digest.sha256());
}

#[rstest]
#[case(String::new(), PlacementKeyError::Empty { field: "backend" })]
#[case("a".repeat(513), PlacementKeyError::TooLong { field: "backend" })]
#[case("a\0b".to_owned(), PlacementKeyError::ContainsNul { field: "backend" })]
fn test_key_component_rejects_bad_values(#[case] value: String, #[case] expected: PlacementKeyError) {
    assert_eq!(BackendId::new(value), Err(expected));
}

#[test]
fn test_key_component_errors_render_a_field_and_reason() {
    assert_eq!(
        PlacementKeyError::Empty { field: "backend" }.to_string(),
        "backend must not be empty"
    );
    assert_eq!(
        PlacementKeyError::TooLong { field: "location" }.to_string(),
        "location must be at most 512 bytes"
    );
    assert_eq!(
        PlacementKeyError::ContainsNul { field: "data center" }.to_string(),
        "data center must not contain a NUL byte"
    );
}

#[test]
fn test_staging_an_absent_placement_creates_a_pending_record() {
    let (_dir, store) = store();
    let placement = key(1, "dc-a", "loc");
    let outcome = store
        .apply_blob_placement(&placement, &BlobPlacementTransition::Stage, 1, 10)
        .unwrap();
    let record = match outcome {
        BlobPlacementOutcome::Applied(record) => record,
        BlobPlacementOutcome::Unchanged(_) => unreachable!("a fresh stage changes state"),
    };
    assert_eq!(record.state, BlobPlacementState::Pending);
    assert_eq!(record.fence, 1);
    assert_eq!(record.generation, 1);
    assert_eq!(record.updated_at_unix, 10);
    assert_eq!(store.blob_placement(&placement).unwrap(), Some(record));
}

#[test]
fn test_restaging_a_pending_placement_is_unchanged() {
    let (_dir, store) = store();
    let placement = key(1, "dc-a", "loc");
    store
        .apply_blob_placement(&placement, &BlobPlacementTransition::Stage, 1, 10)
        .unwrap();
    let outcome = store
        .apply_blob_placement(&placement, &BlobPlacementTransition::Stage, 1, 20)
        .unwrap();
    assert!(matches!(outcome, BlobPlacementOutcome::Unchanged(_)));
    assert_eq!(outcome.record().generation, 1);
    assert_eq!(outcome.record().updated_at_unix, 10);
}

#[test]
fn test_verifying_a_matching_digest_marks_it_verified() {
    let (_dir, store) = store();
    let placement = key(1, "dc-a", "loc");
    store
        .apply_blob_placement(&placement, &BlobPlacementTransition::Stage, 1, 10)
        .unwrap();
    let outcome = store
        .apply_blob_placement(
            &placement,
            &BlobPlacementTransition::Verify {
                observed: digest(1),
                size: 4_096,
            },
            1,
            20,
        )
        .unwrap();
    assert!(matches!(outcome, BlobPlacementOutcome::Applied(_)));
    assert_eq!(outcome.record().state, BlobPlacementState::Verified { size: 4_096 });
    assert_eq!(outcome.record().generation, 2);
}

#[test]
fn test_verifying_a_mismatched_digest_fails_the_candidate() {
    let (_dir, store) = store();
    let placement = key(1, "dc-a", "loc");
    store
        .apply_blob_placement(&placement, &BlobPlacementTransition::Stage, 1, 10)
        .unwrap();
    let outcome = store
        .apply_blob_placement(
            &placement,
            &BlobPlacementTransition::Verify {
                observed: digest(2),
                size: 4_096,
            },
            1,
            20,
        )
        .unwrap();
    assert_eq!(
        outcome.record().state,
        BlobPlacementState::Failed {
            class: BlobPlacementFailure::DigestMismatch,
        }
    );
}

#[test]
fn test_failing_then_restaging_retries_the_transfer() {
    let (_dir, store) = store();
    let placement = key(1, "dc-a", "loc");
    store
        .apply_blob_placement(&placement, &BlobPlacementTransition::Stage, 1, 10)
        .unwrap();
    store
        .apply_blob_placement(
            &placement,
            &BlobPlacementTransition::Fail {
                class: BlobPlacementFailure::SourceUnavailable,
            },
            1,
            20,
        )
        .unwrap();
    let retry_failed = store
        .apply_blob_placement(
            &placement,
            &BlobPlacementTransition::Fail {
                class: BlobPlacementFailure::BackendRejected,
            },
            1,
            25,
        )
        .unwrap();
    assert!(matches!(retry_failed, BlobPlacementOutcome::Unchanged(_)));
    assert_eq!(
        retry_failed.record().state,
        BlobPlacementState::Failed {
            class: BlobPlacementFailure::SourceUnavailable,
        }
    );
    let restaged = store
        .apply_blob_placement(&placement, &BlobPlacementTransition::Stage, 1, 30)
        .unwrap();
    assert_eq!(restaged.record().state, BlobPlacementState::Pending);
    assert_eq!(restaged.record().generation, 3);
}

#[test]
fn test_a_higher_fence_takes_over_and_a_lower_one_is_rejected() {
    let (_dir, store) = store();
    let placement = key(1, "dc-a", "loc");
    store
        .apply_blob_placement(&placement, &BlobPlacementTransition::Stage, 5, 10)
        .unwrap();
    let stale = store
        .apply_blob_placement(&placement, &BlobPlacementTransition::Stage, 3, 20)
        .unwrap_err();
    assert!(matches!(
        stale,
        BlobPlacementError::StaleFence { current: 5, applied: 3 }
    ));
    assert_eq!(stale.to_string(), "a newer fence 5 supersedes the applied fence 3");
    let taken_over = store
        .apply_blob_placement(
            &placement,
            &BlobPlacementTransition::Verify {
                observed: digest(1),
                size: 1,
            },
            9,
            30,
        )
        .unwrap();
    assert_eq!(taken_over.record().fence, 9);
}

#[rstest]
#[case(BlobPlacementState::Pending)]
#[case(BlobPlacementState::Verified { size: 1 })]
#[case(BlobPlacementState::Failed { class: BlobPlacementFailure::SourceUnavailable })]
fn test_revoke_withdraws_any_live_placement(#[case] before: BlobPlacementState) {
    let (_dir, store) = store();
    let placement = key(1, "dc-a", "loc");
    store
        .apply_blob_placement(&placement, &BlobPlacementTransition::Stage, 1, 10)
        .unwrap();
    drive_to(&store, &placement, before);
    let revoked = store
        .apply_blob_placement(&placement, &BlobPlacementTransition::Revoke, 1, 40)
        .unwrap();
    assert_eq!(revoked.record().state, BlobPlacementState::Revoked);
    let again = store
        .apply_blob_placement(&placement, &BlobPlacementTransition::Revoke, 1, 41)
        .unwrap();
    assert!(matches!(again, BlobPlacementOutcome::Unchanged(_)));
}

fn drive_to(store: &MetaStore, placement: &BlobPlacementKey, state: BlobPlacementState) {
    let transition = match state {
        BlobPlacementState::Pending => return,
        BlobPlacementState::Verified { size } => BlobPlacementTransition::Verify {
            observed: placement.digest.clone(),
            size,
        },
        BlobPlacementState::Failed { class } => BlobPlacementTransition::Fail { class },
        BlobPlacementState::Revoked => BlobPlacementTransition::Revoke,
    };
    store.apply_blob_placement(placement, &transition, 1, 30).unwrap();
}

#[rstest]
#[case(BlobPlacementState::Verified { size: 1 }, BlobPlacementTransition::Stage, "stage")]
#[case(BlobPlacementState::Verified { size: 1 }, BlobPlacementTransition::Fail { class: BlobPlacementFailure::SourceUnavailable }, "fail")]
#[case(BlobPlacementState::Verified { size: 1 }, BlobPlacementTransition::Verify { observed: digest(1), size: 1 }, "verify")]
#[case(BlobPlacementState::Revoked, BlobPlacementTransition::Stage, "stage")]
fn test_illegal_transitions_report_the_state_and_step(
    #[case] before: BlobPlacementState,
    #[case] transition: BlobPlacementTransition,
    #[case] step: &str,
) {
    let (_dir, store) = store();
    let placement = key(1, "dc-a", "loc");
    store
        .apply_blob_placement(&placement, &BlobPlacementTransition::Stage, 1, 10)
        .unwrap();
    drive_to(&store, &placement, before);
    let error = store.apply_blob_placement(&placement, &transition, 1, 20).unwrap_err();
    let BlobPlacementError::IllegalTransition {
        from,
        transition: reported,
    } = &error
    else {
        unreachable!("expected an illegal transition, got {error:?}");
    };
    assert_eq!(*from, before.status());
    assert_eq!(*reported, step);
    assert!(error.to_string().contains(step));
}

#[rstest]
#[case(BlobPlacementTransition::Verify { observed: digest(1), size: 1 }, "verify")]
#[case(BlobPlacementTransition::Fail { class: BlobPlacementFailure::SourceUnavailable }, "fail")]
#[case(BlobPlacementTransition::Revoke, "revoke")]
fn test_transitioning_an_absent_placement_reports_it_missing(
    #[case] transition: BlobPlacementTransition,
    #[case] step: &str,
) {
    let (_dir, store) = store();
    let error = store
        .apply_blob_placement(&key(1, "dc-a", "loc"), &transition, 1, 20)
        .unwrap_err();
    let BlobPlacementError::MissingPlacement { transition: reported } = &error else {
        unreachable!("expected a missing placement, got {error:?}");
    };
    assert_eq!(*reported, step);
    assert!(error.to_string().contains(step));
}

#[test]
fn test_an_integrity_failure_demotes_a_verified_copy_to_a_digest_mismatch() {
    let (_dir, store) = store();
    let placement = key(1, "dc-a", "loc");
    store
        .apply_blob_placement(&placement, &BlobPlacementTransition::Stage, 1, 10)
        .unwrap();
    drive_to(&store, &placement, BlobPlacementState::Verified { size: 1 });

    // A verified copy whose stored bytes fail re-verification demotes to a digest mismatch, which makes it
    // a re-copy candidate again rather than a served but corrupt placement.
    let outcome = store
        .apply_blob_placement(
            &placement,
            &BlobPlacementTransition::Fail {
                class: BlobPlacementFailure::DigestMismatch,
            },
            1,
            20,
        )
        .unwrap();

    assert!(matches!(outcome, BlobPlacementOutcome::Applied(_)));
    assert_eq!(
        outcome.record().state,
        BlobPlacementState::Failed {
            class: BlobPlacementFailure::DigestMismatch
        }
    );
}

#[test]
fn test_a_digest_cannot_exceed_its_placement_bound() {
    let (_dir, store) = store();
    for index in 0..MAX_PLACEMENTS_PER_DIGEST {
        store
            .apply_blob_placement(
                &key(1, "dc-a", &format!("loc-{index}")),
                &BlobPlacementTransition::Stage,
                1,
                10,
            )
            .unwrap();
    }
    let error = store
        .apply_blob_placement(&key(1, "dc-a", "loc-overflow"), &BlobPlacementTransition::Stage, 1, 10)
        .unwrap_err();
    assert!(matches!(error, BlobPlacementError::TooManyPlacements));
    assert_eq!(error.to_string(), "a digest cannot exceed 64 placements");
}

#[test]
fn test_reading_an_absent_placement_returns_none() {
    let (_dir, store) = store();
    assert_eq!(store.blob_placement(&key(1, "dc-a", "loc")).unwrap(), None);
    assert!(store.blob_placements(&digest(1)).unwrap().is_empty());
}

#[test]
fn test_digest_placements_list_only_the_queried_digest_in_key_order() {
    let (_dir, store) = store();
    for location in ["b", "a", "c"] {
        store
            .apply_blob_placement(&key(1, "dc-a", location), &BlobPlacementTransition::Stage, 1, 10)
            .unwrap();
    }
    store
        .apply_blob_placement(&key(2, "dc-a", "z"), &BlobPlacementTransition::Stage, 1, 10)
        .unwrap();
    let locations: Vec<String> = store
        .blob_placements(&digest(1))
        .unwrap()
        .into_iter()
        .map(|record| record.key.location.as_str().to_owned())
        .collect();
    assert_eq!(locations, ["a", "b", "c"]);
}

#[test]
fn test_routing_splits_local_remote_pending_failed_and_revoked() {
    let (_dir, store) = store();
    let verify = |placement: &BlobPlacementKey| {
        store
            .apply_blob_placement(placement, &BlobPlacementTransition::Stage, 1, 10)
            .unwrap();
        store
            .apply_blob_placement(
                placement,
                &BlobPlacementTransition::Verify {
                    observed: placement.digest.clone(),
                    size: 1,
                },
                1,
                20,
            )
            .unwrap();
    };
    verify(&key(1, "home", "local"));
    verify(&key(1, "away", "remote"));
    store
        .apply_blob_placement(&key(1, "home", "pending"), &BlobPlacementTransition::Stage, 1, 10)
        .unwrap();
    let failed = key(1, "home", "failed");
    store
        .apply_blob_placement(&failed, &BlobPlacementTransition::Stage, 1, 10)
        .unwrap();
    store
        .apply_blob_placement(
            &failed,
            &BlobPlacementTransition::Fail {
                class: BlobPlacementFailure::BackendRejected,
            },
            1,
            20,
        )
        .unwrap();
    let revoked = key(1, "home", "revoked");
    store
        .apply_blob_placement(&revoked, &BlobPlacementTransition::Stage, 1, 10)
        .unwrap();
    store
        .apply_blob_placement(&revoked, &BlobPlacementTransition::Revoke, 1, 20)
        .unwrap();

    let routing = store.route_blob_placements(&digest(1), &dc("home")).unwrap();
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
    let (_dir, store) = store();
    assert!(store.route_blob_placements(&digest(1), &dc("home")).unwrap().is_empty());
    store
        .apply_blob_placement(&key(1, "home", "pending"), &BlobPlacementTransition::Stage, 1, 10)
        .unwrap();
    let routing = store.route_blob_placements(&digest(1), &dc("home")).unwrap();
    assert!(!routing.is_serveable());
    assert!(!routing.is_empty());
}

#[test]
fn test_status_projects_each_state() {
    assert_eq!(BlobPlacementState::Pending.status(), BlobPlacementStatus::Pending);
    assert_eq!(
        BlobPlacementState::Verified { size: 1 }.status(),
        BlobPlacementStatus::Verified
    );
    assert_eq!(
        BlobPlacementState::Failed {
            class: BlobPlacementFailure::DigestMismatch,
        }
        .status(),
        BlobPlacementStatus::Failed
    );
    assert_eq!(BlobPlacementState::Revoked.status(), BlobPlacementStatus::Revoked);
}

#[test]
fn test_record_local_placement_verifies_the_home_datacenter() {
    let (_dir, store) = store();
    let backend = BackendId::new("filesystem").unwrap();
    let outcome = store
        .record_local_placement(&backend, &dc("home"), &digest(7), 2_048, 3, 100)
        .unwrap();
    assert_eq!(outcome.record().state, BlobPlacementState::Verified { size: 2_048 });
    let routing = store.route_blob_placements(&digest(7), &dc("peer")).unwrap();
    assert_eq!(
        routing.verified_remote.len(),
        1,
        "a peer routes a read-through to the home placement"
    );
    assert_eq!(routing.verified_remote[0].key.data_center, dc("home"));
}

#[test]
fn test_record_local_placement_is_idempotent_on_a_republish() {
    let (_dir, store) = store();
    let backend = BackendId::new("filesystem").unwrap();
    store
        .record_local_placement(&backend, &dc("home"), &digest(7), 2_048, 3, 100)
        .unwrap();
    let outcome = store
        .record_local_placement(&backend, &dc("home"), &digest(7), 2_048, 3, 200)
        .unwrap();
    assert!(
        matches!(outcome, BlobPlacementOutcome::Unchanged(_)),
        "a re-push of an already-verified digest leaves the record as is",
    );
}

#[test]
fn test_record_local_placement_recovers_a_failed_placement() {
    // A placement that failed an integrity check re-stages and verifies when the home republishes it.
    let (_dir, store) = store();
    let backend = BackendId::new("filesystem").unwrap();
    let placement = BlobPlacementKey {
        digest: digest(7),
        backend: backend.clone(),
        data_center: dc("home"),
        location: BackendLocation::for_digest(&digest(7)),
    };
    store
        .apply_blob_placement(&placement, &BlobPlacementTransition::Stage, 3, 10)
        .unwrap();
    store
        .apply_blob_placement(
            &placement,
            &BlobPlacementTransition::Fail {
                class: BlobPlacementFailure::DigestMismatch,
            },
            3,
            20,
        )
        .unwrap();
    let outcome = store
        .record_local_placement(&backend, &dc("home"), &digest(7), 2_048, 3, 100)
        .unwrap();
    assert_eq!(outcome.record().state, BlobPlacementState::Verified { size: 2_048 });
}

#[test]
fn test_record_local_placement_rejects_a_stale_fence() {
    let (_dir, store) = store();
    let backend = BackendId::new("filesystem").unwrap();
    let placement = BlobPlacementKey {
        digest: digest(7),
        backend: backend.clone(),
        data_center: dc("home"),
        location: BackendLocation::for_digest(&digest(7)),
    };
    store
        .apply_blob_placement(&placement, &BlobPlacementTransition::Stage, 5, 10)
        .unwrap();
    let error = store
        .record_local_placement(&backend, &dc("home"), &digest(7), 2_048, 2, 200)
        .unwrap_err();
    assert!(matches!(error, BlobPlacementError::StaleFence { .. }));
}
