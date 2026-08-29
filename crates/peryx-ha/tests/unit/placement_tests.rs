use std::str::FromStr as _;

use peryx_identity::ArtifactDigest;
use rstest::rstest;

use crate::{
    BackendId, BackendLocation, BlobPlacementDecisionError, BlobPlacementFailure, BlobPlacementKey,
    BlobPlacementOutcome, BlobPlacementRecord, BlobPlacementState, BlobPlacementStatus, BlobPlacementTransition,
    DataCenterId, PlacementKeyError, decide_blob_placement,
};

fn digest(suffix: u8) -> ArtifactDigest {
    ArtifactDigest::from_str(&format!("sha256:{suffix:064x}")).unwrap()
}

fn key() -> BlobPlacementKey {
    BlobPlacementKey {
        digest: digest(1),
        backend: BackendId::new("filesystem").unwrap(),
        data_center: DataCenterId::new("east").unwrap(),
        location: BackendLocation::new("east/01").unwrap(),
    }
}

fn record(state: BlobPlacementState, fence: u64, generation: u64) -> BlobPlacementRecord {
    BlobPlacementRecord {
        key: key(),
        state,
        fence,
        transfer_attempt: 1,
        generation,
        updated_at_unix: 10,
    }
}

#[rstest]
#[case::backend(BackendId::new("s3").map(|value| value.as_str().to_owned()), Ok("s3".to_owned()))]
#[case::data_center(DataCenterId::new("dc-1").map(|value| value.as_str().to_owned()), Ok("dc-1".to_owned()))]
#[case::location(BackendLocation::new("blobs/aa").map(|value| value.as_str().to_owned()), Ok("blobs/aa".to_owned()))]
#[case::maximum_length(
    BackendId::new("a".repeat(512)).map(|value| value.as_str().to_owned()),
    Ok("a".repeat(512))
)]
fn test_key_components_accept_valid_values(
    #[case] result: Result<String, PlacementKeyError>,
    #[case] expected: Result<String, PlacementKeyError>,
) {
    assert_eq!(result, expected);
}

#[rstest]
#[case::empty(String::new(), PlacementKeyError::Empty { field: "backend" })]
#[case::long("a".repeat(513), PlacementKeyError::TooLong { field: "backend" })]
#[case::nul("a\0b".to_owned(), PlacementKeyError::ContainsNul { field: "backend" })]
fn test_key_components_reject_invalid_values(#[case] value: String, #[case] expected: PlacementKeyError) {
    assert_eq!(BackendId::new(value), Err(expected));
}

#[test]
fn test_digest_location_uses_the_sha256_value() {
    assert_eq!(BackendLocation::for_digest(&digest(42)).as_str(), digest(42).sha256());
}

#[test]
fn test_stage_creates_a_pending_record() {
    assert_eq!(
        decide_blob_placement(&key(), None, &BlobPlacementTransition::Stage, 3, 20).unwrap(),
        BlobPlacementOutcome::Applied(BlobPlacementRecord {
            key: key(),
            state: BlobPlacementState::Pending,
            fence: 3,
            transfer_attempt: 1,
            generation: 1,
            updated_at_unix: 20,
        })
    );
}

#[test]
fn test_placement_without_an_attempt_reads_as_legacy_state() {
    let mut encoded = serde_json::to_value(record(BlobPlacementState::Pending, 3, 1)).unwrap();
    encoded.as_object_mut().unwrap().remove("transfer_attempt");
    let mut expected = record(BlobPlacementState::Pending, 3, 1);
    expected.transfer_attempt = 0;

    assert_eq!(
        serde_json::from_value::<BlobPlacementRecord>(encoded).unwrap(),
        expected
    );
}

#[test]
fn test_restaging_pending_is_unchanged() {
    let prior = record(BlobPlacementState::Pending, 3, 1);

    assert_eq!(
        decide_blob_placement(&key(), Some(&prior), &BlobPlacementTransition::Stage, 3, 20).unwrap(),
        BlobPlacementOutcome::Unchanged(prior)
    );
}

#[rstest]
#[case::matching(digest(1), BlobPlacementState::Verified { size: 4_096 })]
#[case::mismatch(
    digest(2),
    BlobPlacementState::Failed { class: BlobPlacementFailure::DigestMismatch }
)]
fn test_verify_projects_digest_evidence(#[case] observed: ArtifactDigest, #[case] expected: BlobPlacementState) {
    let prior = record(BlobPlacementState::Pending, 3, 1);
    let outcome = decide_blob_placement(
        &key(),
        Some(&prior),
        &BlobPlacementTransition::Verify {
            attempt: 1,
            observed,
            size: 4_096,
        },
        3,
        20,
    )
    .unwrap();

    assert_eq!(outcome.record().state, expected);
    assert_eq!(outcome.record().generation, 2);
}

#[test]
fn test_failed_placement_can_be_restaged() {
    let prior = record(
        BlobPlacementState::Failed {
            class: BlobPlacementFailure::SourceUnavailable,
        },
        3,
        2,
    );

    assert_eq!(
        decide_blob_placement(&key(), Some(&prior), &BlobPlacementTransition::Stage, 3, 30)
            .unwrap()
            .record(),
        &BlobPlacementRecord {
            key: key(),
            state: BlobPlacementState::Pending,
            fence: 3,
            transfer_attempt: 2,
            generation: 3,
            updated_at_unix: 30,
        }
    );
}

#[test]
fn test_repeated_failure_is_unchanged() {
    let prior = record(
        BlobPlacementState::Failed {
            class: BlobPlacementFailure::SourceUnavailable,
        },
        3,
        2,
    );

    assert_eq!(
        decide_blob_placement(
            &key(),
            Some(&prior),
            &BlobPlacementTransition::Fail {
                attempt: 1,
                class: BlobPlacementFailure::BackendRejected,
            },
            3,
            30,
        )
        .unwrap(),
        BlobPlacementOutcome::Unchanged(prior)
    );
}

#[test]
fn test_pending_failure_records_the_failure() {
    assert_eq!(
        decide_blob_placement(
            &key(),
            Some(&record(BlobPlacementState::Pending, 3, 1)),
            &BlobPlacementTransition::Fail {
                attempt: 1,
                class: BlobPlacementFailure::SourceUnavailable,
            },
            3,
            30,
        )
        .unwrap(),
        BlobPlacementOutcome::Applied(BlobPlacementRecord {
            key: key(),
            state: BlobPlacementState::Failed {
                class: BlobPlacementFailure::SourceUnavailable,
            },
            fence: 3,
            transfer_attempt: 1,
            generation: 2,
            updated_at_unix: 30,
        })
    );
}

#[test]
fn test_fence_rejects_stale_writes() {
    let prior = record(BlobPlacementState::Pending, 5, 1);
    assert_eq!(
        decide_blob_placement(&key(), Some(&prior), &BlobPlacementTransition::Stage, 3, 20),
        Err(BlobPlacementDecisionError::StaleFence { current: 5, applied: 3 })
    );
}

#[test]
fn test_newer_fence_claims_pending_placement() {
    let prior = record(BlobPlacementState::Pending, 5, 1);
    assert_eq!(
        decide_blob_placement(&key(), Some(&prior), &BlobPlacementTransition::Stage, 9, 30).unwrap(),
        BlobPlacementOutcome::Applied(BlobPlacementRecord {
            key: key(),
            state: BlobPlacementState::Pending,
            fence: 9,
            transfer_attempt: 2,
            generation: 2,
            updated_at_unix: 30,
        })
    );
}

#[rstest]
#[case::checkpoint(BlobPlacementTransition::Checkpoint { attempt: 1 })]
#[case::verify(BlobPlacementTransition::Verify { attempt: 1, observed: digest(1), size: 1 })]
#[case::fail(BlobPlacementTransition::Fail {
    attempt: 1,
    class: BlobPlacementFailure::SourceUnavailable,
})]
fn test_stale_attempt_cannot_mutate_a_replacement(#[case] transition: BlobPlacementTransition) {
    let mut prior = record(BlobPlacementState::Pending, 9, 2);
    prior.transfer_attempt = 2;

    assert_eq!(
        decide_blob_placement(&key(), Some(&prior), &transition, 9, 30),
        Err(BlobPlacementDecisionError::StaleTransferAttempt { current: 2, applied: 1 })
    );
}

#[test]
fn test_attempt_write_requires_the_claimed_fence() {
    assert_eq!(
        decide_blob_placement(
            &key(),
            Some(&record(BlobPlacementState::Pending, 5, 1)),
            &BlobPlacementTransition::Checkpoint { attempt: 1 },
            9,
            30,
        ),
        Err(BlobPlacementDecisionError::TransferAttemptFenceMismatch { current: 5, applied: 9 })
    );
}

#[test]
fn test_checkpoint_renews_the_claimed_attempt() {
    assert_eq!(
        decide_blob_placement(
            &key(),
            Some(&record(BlobPlacementState::Pending, 5, 1)),
            &BlobPlacementTransition::Checkpoint { attempt: 1 },
            5,
            30,
        )
        .unwrap()
        .record(),
        &BlobPlacementRecord {
            key: key(),
            state: BlobPlacementState::Pending,
            fence: 5,
            transfer_attempt: 1,
            generation: 2,
            updated_at_unix: 30,
        }
    );
}

#[test]
fn test_exhausted_attempt_cannot_be_reclaimed() {
    let mut prior = record(BlobPlacementState::Pending, 5, 1);
    prior.transfer_attempt = u64::MAX;

    assert_eq!(
        decide_blob_placement(&key(), Some(&prior), &BlobPlacementTransition::Stage, 9, 30),
        Err(BlobPlacementDecisionError::TransferAttemptExhausted)
    );
}

#[rstest]
#[case::pending(BlobPlacementState::Pending)]
#[case::verified(BlobPlacementState::Verified { size: 1 })]
#[case::failed(BlobPlacementState::Failed { class: BlobPlacementFailure::SourceUnavailable })]
#[case::revoked(BlobPlacementState::Revoked)]
fn test_revoke_withdraws_or_preserves_a_revoked_record(#[case] state: BlobPlacementState) {
    let prior = record(state, 3, 2);
    let outcome = decide_blob_placement(&key(), Some(&prior), &BlobPlacementTransition::Revoke, 3, 30).unwrap();

    assert_eq!(outcome.record().state, BlobPlacementState::Revoked);
    assert_eq!(
        matches!(outcome, BlobPlacementOutcome::Unchanged(_)),
        state == BlobPlacementState::Revoked
    );
}

#[rstest]
#[case::stage_verified(BlobPlacementState::Verified { size: 1 }, BlobPlacementTransition::Stage)]
#[case::fail_verified(
    BlobPlacementState::Verified { size: 1 },
    BlobPlacementTransition::Fail { attempt: 1, class: BlobPlacementFailure::SourceUnavailable }
)]
#[case::verify_verified(
    BlobPlacementState::Verified { size: 1 },
    BlobPlacementTransition::Verify { attempt: 1, observed: digest(1), size: 1 }
)]
#[case::stage_revoked(BlobPlacementState::Revoked, BlobPlacementTransition::Stage)]
fn test_illegal_transitions_report_the_state_and_step(
    #[case] state: BlobPlacementState,
    #[case] transition: BlobPlacementTransition,
) {
    assert_eq!(
        decide_blob_placement(&key(), Some(&record(state, 3, 2)), &transition, 3, 30),
        Err(BlobPlacementDecisionError::IllegalTransition {
            from: state.status(),
            transition: transition.label(),
        })
    );
}

#[rstest]
#[case::checkpoint(BlobPlacementTransition::Checkpoint { attempt: 1 })]
#[case::verify(BlobPlacementTransition::Verify { attempt: 1, observed: digest(1), size: 1 })]
#[case::fail(BlobPlacementTransition::Fail { attempt: 1, class: BlobPlacementFailure::SourceUnavailable })]
#[case::invalidate(BlobPlacementTransition::Invalidate)]
#[case::revoke(BlobPlacementTransition::Revoke)]
fn test_non_stage_transition_requires_a_record(#[case] transition: BlobPlacementTransition) {
    assert_eq!(
        decide_blob_placement(&key(), None, &transition, 3, 30),
        Err(BlobPlacementDecisionError::MissingPlacement {
            transition: transition.label(),
        })
    );
}

#[rstest]
#[case::stage(BlobPlacementTransition::Stage, "stage")]
#[case::checkpoint(BlobPlacementTransition::Checkpoint { attempt: 1 }, "checkpoint")]
#[case::verify(BlobPlacementTransition::Verify { attempt: 1, observed: digest(1), size: 1 }, "verify")]
#[case::fail(
    BlobPlacementTransition::Fail { attempt: 1, class: BlobPlacementFailure::SourceUnavailable },
    "fail"
)]
#[case::invalidate(BlobPlacementTransition::Invalidate, "invalidate")]
#[case::revoke(BlobPlacementTransition::Revoke, "revoke")]
fn test_transition_labels_name_the_operation(#[case] transition: BlobPlacementTransition, #[case] expected: &str) {
    assert_eq!(transition.label(), expected);
}

#[test]
fn test_digest_failure_demotes_a_verified_record() {
    let prior = record(BlobPlacementState::Verified { size: 1 }, 3, 2);

    assert_eq!(
        decide_blob_placement(&key(), Some(&prior), &BlobPlacementTransition::Invalidate, 3, 30,)
            .unwrap()
            .record()
            .state,
        BlobPlacementState::Failed {
            class: BlobPlacementFailure::DigestMismatch,
        }
    );
}

#[rstest]
#[case::pending(BlobPlacementState::Pending, BlobPlacementStatus::Pending)]
#[case::verified(BlobPlacementState::Verified { size: 1 }, BlobPlacementStatus::Verified)]
#[case::failed(
    BlobPlacementState::Failed { class: BlobPlacementFailure::DigestMismatch },
    BlobPlacementStatus::Failed
)]
#[case::revoked(BlobPlacementState::Revoked, BlobPlacementStatus::Revoked)]
fn test_state_projects_its_status(#[case] state: BlobPlacementState, #[case] expected: BlobPlacementStatus) {
    assert_eq!(state.status(), expected);
}
