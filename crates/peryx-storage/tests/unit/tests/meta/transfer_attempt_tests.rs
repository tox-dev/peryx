use std::str::FromStr as _;

use peryx_identity::ArtifactDigest;
use rstest::rstest;
use tempfile::TempDir;

use crate::meta::{
    AttemptRetention, BackendId, BackendLocation, BeginOutcome, BlobPlacementFailure, BlobPlacementKey,
    CheckpointOutcome, CheckpointPolicy, DataCenterId, MAX_ATTEMPTS_PER_PLACEMENT, MetaStore, TransferAttemptError,
    TransferAttemptMetric, TransferAttemptState, TransferAttemptStatus, TransferPlan,
};

const SIZE: u64 = 1_000;

fn store() -> (TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, store)
}

fn digest(suffix: u8) -> ArtifactDigest {
    ArtifactDigest::from_str(&format!("sha256:{suffix:064x}")).unwrap()
}

fn key(suffix: u8, backend: &str, data_center: &str, location: &str) -> BlobPlacementKey {
    BlobPlacementKey {
        digest: digest(suffix),
        backend: BackendId::new(backend).unwrap(),
        data_center: DataCenterId::new(data_center).unwrap(),
        location: BackendLocation::new(location).unwrap(),
    }
}

fn target(suffix: u8) -> BlobPlacementKey {
    key(suffix, "filesystem", "dc-1", "blobs/aa")
}

fn plan() -> TransferPlan {
    TransferPlan {
        expected_size: SIZE,
        source_data_center: None,
    }
}

fn policy() -> CheckpointPolicy {
    CheckpointPolicy {
        min_bytes: 100,
        min_interval_secs: 60,
    }
}

fn begin(store: &MetaStore, key: &BlobPlacementKey, fence: u64, now: i64) -> BeginOutcome {
    store.begin_transfer_attempt(key, &plan(), fence, now).unwrap()
}

#[test]
fn test_begin_opens_the_first_attempt_in_progress_at_zero() {
    let (_dir, store) = store();
    let outcome = begin(&store, &target(1), 1, 10);
    let record = outcome.record();
    assert!(matches!(outcome, BeginOutcome::Started(_)));
    assert_eq!(record.sequence, 1);
    assert_eq!(record.state, TransferAttemptState::InProgress { transferred: 0 });
    assert_eq!(record.expected_size, SIZE);
    assert_eq!(record.fence, 1);
    assert_eq!(record.started_at_unix, 10);
}

#[test]
fn test_begin_records_the_reselected_source_data_center() {
    let (_dir, store) = store();
    let plan = TransferPlan {
        expected_size: SIZE,
        source_data_center: Some(DataCenterId::new("dc-remote").unwrap()),
    };
    store.begin_transfer_attempt(&target(1), &plan, 1, 0).unwrap();
    let record = store.transfer_attempt(&target(1)).unwrap().unwrap();
    assert_eq!(record.source_data_center, Some(DataCenterId::new("dc-remote").unwrap()));
}

#[test]
fn test_begin_resumes_an_in_progress_attempt_without_opening_a_new_one() {
    let (_dir, store) = store();
    let key = target(1);
    begin(&store, &key, 1, 0);
    store.checkpoint_transfer_attempt(&key, 300, policy(), 1, 5).unwrap();
    let outcome = begin(&store, &key, 1, 20);
    assert!(matches!(outcome, BeginOutcome::Resumed(_)));
    assert_eq!(outcome.record().sequence, 1);
    assert_eq!(
        outcome.record().state,
        TransferAttemptState::InProgress { transferred: 300 }
    );
}

#[test]
fn test_resume_by_a_newer_fence_fences_out_the_superseded_worker() {
    let (_dir, store) = store();
    let key = target(1);
    begin(&store, &key, 1, 0);
    store.checkpoint_transfer_attempt(&key, 300, policy(), 1, 5).unwrap();
    let resumed = begin(&store, &key, 2, 10);
    assert!(matches!(resumed, BeginOutcome::Resumed(_)));
    assert_eq!(resumed.record().fence, 2);
    assert_eq!(store.transfer_attempt(&key).unwrap().unwrap().fence, 2);
    let error = store
        .checkpoint_transfer_attempt(&key, 600, policy(), 1, 15)
        .unwrap_err();
    assert!(matches!(error, TransferAttemptError::StaleFence { .. }));
    assert_eq!(
        store.transfer_attempt(&key).unwrap().unwrap().state,
        TransferAttemptState::InProgress { transferred: 300 }
    );
}

#[test]
fn test_resume_at_the_same_fence_leaves_the_record_untouched() {
    let (_dir, store) = store();
    let key = target(1);
    begin(&store, &key, 1, 0);
    store.checkpoint_transfer_attempt(&key, 300, policy(), 1, 5).unwrap();
    let before = store.transfer_attempt(&key).unwrap().unwrap();
    let resumed = begin(&store, &key, 1, 20);
    assert!(matches!(resumed, BeginOutcome::Resumed(_)));
    assert_eq!(store.transfer_attempt(&key).unwrap().unwrap(), before);
}

#[test]
fn test_begin_after_a_failure_opens_the_next_sequence() {
    let (_dir, store) = store();
    let key = target(1);
    begin(&store, &key, 1, 0);
    store
        .fail_transfer_attempt(&key, BlobPlacementFailure::SourceUnavailable, 1, 5)
        .unwrap();
    let outcome = begin(&store, &key, 1, 10);
    assert!(matches!(outcome, BeginOutcome::Started(_)));
    assert_eq!(outcome.record().sequence, 2);
}

#[test]
fn test_begin_after_success_is_rejected() {
    let (_dir, store) = store();
    let key = target(1);
    begin(&store, &key, 1, 0);
    store.complete_transfer_attempt(&key, &digest(1), SIZE, 1, 5).unwrap();
    let error = store.begin_transfer_attempt(&key, &plan(), 1, 10).unwrap_err();
    assert!(matches!(error, TransferAttemptError::AlreadySucceeded));
}

#[test]
fn test_begin_with_a_stale_fence_is_rejected() {
    let (_dir, store) = store();
    let key = target(1);
    begin(&store, &key, 5, 0);
    store
        .fail_transfer_attempt(&key, BlobPlacementFailure::SourceUnavailable, 5, 5)
        .unwrap();
    let error = store.begin_transfer_attempt(&key, &plan(), 3, 10).unwrap_err();
    assert!(matches!(
        error,
        TransferAttemptError::StaleFence { current: 5, applied: 3 }
    ));
}

#[test]
fn test_begin_is_bounded_by_the_per_placement_attempt_cap() {
    let (_dir, store) = store();
    let key = target(1);
    for round in 0..MAX_ATTEMPTS_PER_PLACEMENT {
        let now = i64::try_from(round).unwrap();
        begin(&store, &key, 1, now);
        store
            .fail_transfer_attempt(&key, BlobPlacementFailure::SourceUnavailable, 1, now)
            .unwrap();
    }
    let error = store.begin_transfer_attempt(&key, &plan(), 1, 999).unwrap_err();
    assert!(matches!(error, TransferAttemptError::TooManyAttempts));
}

#[rstest]
#[case(50, 1, false)]
#[case(100, 1, true)]
#[case(SIZE, 1, true)]
fn test_checkpoint_persists_only_past_the_byte_budget(#[case] offset: u64, #[case] now: i64, #[case] persisted: bool) {
    let (_dir, store) = store();
    let key = target(1);
    begin(&store, &key, 1, 0);
    let outcome = store
        .checkpoint_transfer_attempt(&key, offset, policy(), 1, now)
        .unwrap();
    assert_eq!(matches!(outcome, CheckpointOutcome::Persisted(_)), persisted);
    let durable = if persisted { offset } else { 0 };
    assert_eq!(
        store.transfer_attempt(&key).unwrap().unwrap().state,
        TransferAttemptState::InProgress { transferred: durable }
    );
}

#[test]
fn test_checkpoint_persists_once_the_interval_elapses() {
    let (_dir, store) = store();
    let key = target(1);
    begin(&store, &key, 1, 0);
    let outcome = store.checkpoint_transfer_attempt(&key, 40, policy(), 1, 61).unwrap();
    assert!(matches!(outcome, CheckpointOutcome::Persisted(_)));
    assert_eq!(outcome.record().checkpointed_at_unix, 61);
}

#[test]
fn test_checkpoint_never_regresses_the_durable_offset() {
    let (_dir, store) = store();
    let key = target(1);
    begin(&store, &key, 1, 0);
    store.checkpoint_transfer_attempt(&key, 400, policy(), 1, 5).unwrap();
    let outcome = store.checkpoint_transfer_attempt(&key, 100, policy(), 1, 200).unwrap();
    assert!(matches!(outcome, CheckpointOutcome::Coalesced(_)));
    assert_eq!(
        outcome.record().state,
        TransferAttemptState::InProgress { transferred: 400 }
    );
}

#[test]
fn test_checkpoint_reaches_the_exact_object_size() {
    let (_dir, store) = store();
    let key = target(1);
    begin(&store, &key, 1, 0);
    store.checkpoint_transfer_attempt(&key, 990, policy(), 1, 5).unwrap();
    store.checkpoint_transfer_attempt(&key, SIZE, policy(), 1, 6).unwrap();
    assert_eq!(
        store.transfer_attempt(&key).unwrap().unwrap().state,
        TransferAttemptState::InProgress { transferred: SIZE }
    );
}

#[test]
fn test_checkpoint_past_the_end_is_rejected() {
    let (_dir, store) = store();
    let key = target(1);
    begin(&store, &key, 1, 0);
    let error = store
        .checkpoint_transfer_attempt(&key, SIZE + 1, policy(), 1, 5)
        .unwrap_err();
    assert!(matches!(
        error,
        TransferAttemptError::OffsetPastEnd {
            offset,
            expected_size: SIZE
        } if offset == SIZE + 1
    ));
}

#[test]
fn test_checkpoint_against_a_failed_attempt_is_rejected() {
    let (_dir, store) = store();
    let key = target(1);
    begin(&store, &key, 1, 0);
    store
        .fail_transfer_attempt(&key, BlobPlacementFailure::SourceUnavailable, 1, 5)
        .unwrap();
    let error = store
        .checkpoint_transfer_attempt(&key, 100, policy(), 1, 6)
        .unwrap_err();
    assert!(matches!(error, TransferAttemptError::NoOpenAttempt));
}

#[test]
fn test_checkpoint_before_any_attempt_is_rejected() {
    let (_dir, store) = store();
    let error = store
        .checkpoint_transfer_attempt(&target(1), 100, policy(), 1, 0)
        .unwrap_err();
    assert!(matches!(error, TransferAttemptError::NoOpenAttempt));
}

#[test]
fn test_checkpoint_with_a_stale_fence_is_rejected() {
    let (_dir, store) = store();
    let key = target(1);
    begin(&store, &key, 5, 0);
    let error = store
        .checkpoint_transfer_attempt(&key, 200, policy(), 3, 5)
        .unwrap_err();
    assert!(matches!(error, TransferAttemptError::StaleFence { .. }));
}

#[test]
fn test_checkpoint_by_a_newer_fence_fences_out_the_superseded_worker() {
    let (_dir, store) = store();
    let key = target(1);
    begin(&store, &key, 1, 0);
    let promoted = store.checkpoint_transfer_attempt(&key, 500, policy(), 2, 5).unwrap();
    assert_eq!(
        promoted.record().state,
        TransferAttemptState::InProgress { transferred: 500 }
    );
    let error = store
        .checkpoint_transfer_attempt(&key, SIZE, policy(), 1, 6)
        .unwrap_err();
    assert!(matches!(error, TransferAttemptError::StaleFence { .. }));
    assert_eq!(
        store.transfer_attempt(&key).unwrap().unwrap().state,
        TransferAttemptState::InProgress { transferred: 500 }
    );
}

#[test]
fn test_checkpoint_cannot_regress_a_completed_attempt_from_a_stale_worker() {
    let (_dir, store) = store();
    let key = target(1);
    begin(&store, &key, 1, 0);
    store.complete_transfer_attempt(&key, &digest(1), SIZE, 2, 5).unwrap();
    let error = store
        .checkpoint_transfer_attempt(&key, SIZE, policy(), 1, 6)
        .unwrap_err();
    assert!(matches!(error, TransferAttemptError::StaleFence { .. }));
    assert_eq!(
        store.transfer_attempt(&key).unwrap().unwrap().state,
        TransferAttemptState::Succeeded { size: SIZE }
    );
}

#[test]
fn test_fail_records_a_classified_terminal_state() {
    let (_dir, store) = store();
    let key = target(1);
    begin(&store, &key, 1, 0);
    let record = store
        .fail_transfer_attempt(&key, BlobPlacementFailure::BackendRejected, 1, 5)
        .unwrap();
    assert_eq!(
        record.state,
        TransferAttemptState::Failed {
            class: BlobPlacementFailure::BackendRejected
        }
    );
}

#[test]
fn test_fail_is_idempotent_against_an_already_failed_attempt() {
    let (_dir, store) = store();
    let key = target(1);
    begin(&store, &key, 1, 0);
    store
        .fail_transfer_attempt(&key, BlobPlacementFailure::SourceUnavailable, 1, 5)
        .unwrap();
    let record = store
        .fail_transfer_attempt(&key, BlobPlacementFailure::BackendRejected, 1, 6)
        .unwrap();
    assert_eq!(
        record.state,
        TransferAttemptState::Failed {
            class: BlobPlacementFailure::SourceUnavailable
        }
    );
}

#[test]
fn test_fail_without_an_attempt_is_rejected() {
    let (_dir, store) = store();
    let error = store
        .fail_transfer_attempt(&target(1), BlobPlacementFailure::SourceUnavailable, 1, 0)
        .unwrap_err();
    assert!(matches!(error, TransferAttemptError::NoOpenAttempt));
}

#[test]
fn test_fail_after_success_is_rejected() {
    let (_dir, store) = store();
    let key = target(1);
    begin(&store, &key, 1, 0);
    store.complete_transfer_attempt(&key, &digest(1), SIZE, 1, 5).unwrap();
    let error = store
        .fail_transfer_attempt(&key, BlobPlacementFailure::SourceUnavailable, 1, 6)
        .unwrap_err();
    assert!(matches!(error, TransferAttemptError::AlreadySucceeded));
}

#[test]
fn test_complete_with_a_matching_digest_and_size_succeeds() {
    let (_dir, store) = store();
    let key = target(1);
    begin(&store, &key, 1, 0);
    let record = store.complete_transfer_attempt(&key, &digest(1), SIZE, 1, 5).unwrap();
    assert_eq!(record.state, TransferAttemptState::Succeeded { size: SIZE });
}

#[rstest]
#[case(digest(2), SIZE)]
#[case(digest(1), SIZE - 1)]
fn test_complete_with_a_mismatch_fails_instead_of_serving(#[case] observed: ArtifactDigest, #[case] size: u64) {
    let (_dir, store) = store();
    let key = target(1);
    begin(&store, &key, 1, 0);
    let record = store.complete_transfer_attempt(&key, &observed, size, 1, 5).unwrap();
    assert_eq!(
        record.state,
        TransferAttemptState::Failed {
            class: BlobPlacementFailure::DigestMismatch
        }
    );
}

#[test]
fn test_complete_with_a_stale_fence_is_rejected() {
    let (_dir, store) = store();
    let key = target(1);
    begin(&store, &key, 5, 0);
    let error = store
        .complete_transfer_attempt(&key, &digest(1), SIZE, 3, 5)
        .unwrap_err();
    assert!(matches!(error, TransferAttemptError::StaleFence { .. }));
}

#[test]
fn test_transfer_attempt_is_absent_before_the_first_begin() {
    let (_dir, store) = store();
    assert!(store.transfer_attempt(&target(1)).unwrap().is_none());
}

#[test]
fn test_transfer_attempts_list_a_digest_history_across_placements() {
    let (_dir, store) = store();
    let first = key(1, "filesystem", "dc-1", "blobs/aa");
    let second = key(1, "s3", "dc-2", "blobs/bb");
    begin(&store, &first, 1, 0);
    store
        .fail_transfer_attempt(&first, BlobPlacementFailure::SourceUnavailable, 1, 1)
        .unwrap();
    begin(&store, &first, 1, 2);
    begin(&store, &second, 1, 3);
    let history = store.transfer_attempts(&digest(1)).unwrap();
    assert_eq!(history.len(), 3);
    assert_eq!(history[0].key.backend.as_str(), "filesystem");
    assert_eq!(history[0].sequence, 1);
    assert_eq!(history[1].sequence, 2);
    assert_eq!(history[2].key.backend.as_str(), "s3");
}

#[test]
fn test_a_reopened_store_resumes_from_the_last_durable_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let key = target(1);
    {
        let store = MetaStore::open(&path).unwrap();
        begin(&store, &key, 1, 0);
        store.checkpoint_transfer_attempt(&key, 512, policy(), 1, 5).unwrap();
    }
    let reopened = MetaStore::open(&path).unwrap();
    let outcome = begin(&reopened, &key, 1, 100);
    assert!(matches!(outcome, BeginOutcome::Resumed(_)));
    assert_eq!(
        outcome.record().state,
        TransferAttemptState::InProgress { transferred: 512 }
    );
}

fn seed_terminal_history(store: &MetaStore, key: &BlobPlacementKey, fail_times: &[i64]) {
    for (round, now) in fail_times.iter().enumerate() {
        begin(store, key, 1, i64::try_from(round).unwrap());
        store
            .fail_transfer_attempt(key, BlobPlacementFailure::SourceUnavailable, 1, *now)
            .unwrap();
    }
}

#[test]
fn test_compaction_prunes_old_terminal_attempts_beyond_the_retained_count() {
    let (_dir, store) = store();
    let key = target(1);
    seed_terminal_history(&store, &key, &[100, 200, 300]);
    begin(&store, &key, 1, 400);
    let retention = AttemptRetention {
        max_age_secs: 10,
        keep_per_placement: 1,
    };
    let removed = store.compact_transfer_attempts(retention, 1_000).unwrap();
    assert_eq!(removed, 2);
    let remaining = store.transfer_attempts(&digest(1)).unwrap();
    assert_eq!(remaining.len(), 2);
    assert_eq!(remaining[0].sequence, 3);
    assert_eq!(remaining[1].state, TransferAttemptState::InProgress { transferred: 0 });
}

#[test]
fn test_compaction_keeps_terminal_attempts_within_the_age_window() {
    let (_dir, store) = store();
    let key = target(1);
    seed_terminal_history(&store, &key, &[100, 200, 300]);
    let retention = AttemptRetention {
        max_age_secs: 100_000,
        keep_per_placement: 1,
    };
    assert_eq!(store.compact_transfer_attempts(retention, 1_000).unwrap(), 0);
}

#[test]
fn test_compaction_removes_every_prunable_attempt() {
    let (_dir, store) = store();
    let key = target(1);
    seed_terminal_history(&store, &key, &[100, 200, 300, 400, 500, 600]);
    let retention = AttemptRetention {
        max_age_secs: 10,
        keep_per_placement: 1,
    };
    assert_eq!(store.compact_transfer_attempts(retention, 10_000).unwrap(), 5);
    let remaining = store.transfer_attempts(&digest(1)).unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].sequence, 6);
}

#[test]
fn test_metrics_count_by_data_center_backend_state_and_error_class() {
    let (_dir, store) = store();
    let one = key(1, "filesystem", "dc-1", "blobs/aa");
    let two = key(2, "filesystem", "dc-1", "blobs/bb");
    let three = key(3, "s3", "dc-2", "blobs/cc");
    begin(&store, &one, 1, 0);
    store.complete_transfer_attempt(&one, &digest(1), SIZE, 1, 1).unwrap();
    begin(&store, &two, 1, 0);
    store
        .fail_transfer_attempt(&two, BlobPlacementFailure::SourceUnavailable, 1, 1)
        .unwrap();
    begin(&store, &three, 1, 0);
    let metrics = store.transfer_attempt_metrics().unwrap();
    assert_eq!(
        metrics,
        vec![
            TransferAttemptMetric {
                data_center: "dc-1".to_owned(),
                backend: "filesystem".to_owned(),
                state: TransferAttemptStatus::Failed,
                error_class: Some(BlobPlacementFailure::SourceUnavailable),
                count: 1,
            },
            TransferAttemptMetric {
                data_center: "dc-1".to_owned(),
                backend: "filesystem".to_owned(),
                state: TransferAttemptStatus::Succeeded,
                error_class: None,
                count: 1,
            },
            TransferAttemptMetric {
                data_center: "dc-2".to_owned(),
                backend: "s3".to_owned(),
                state: TransferAttemptStatus::InProgress,
                error_class: None,
                count: 1,
            },
        ]
    );
}

#[test]
fn test_metrics_omit_the_digest_from_labels() {
    let (_dir, store) = store();
    begin(&store, &key(1, "filesystem", "dc-1", "blobs/aa"), 1, 0);
    begin(&store, &key(2, "filesystem", "dc-1", "blobs/bb"), 1, 0);
    let metrics = store.transfer_attempt_metrics().unwrap();
    assert_eq!(metrics.len(), 1);
    assert_eq!(metrics[0].count, 2);
}

#[test]
fn test_state_status_maps_each_variant() {
    assert_eq!(
        TransferAttemptState::InProgress { transferred: 0 }.status(),
        TransferAttemptStatus::InProgress
    );
    assert_eq!(
        TransferAttemptState::Succeeded { size: SIZE }.status(),
        TransferAttemptStatus::Succeeded
    );
    assert_eq!(
        TransferAttemptState::Failed {
            class: BlobPlacementFailure::DigestMismatch
        }
        .status(),
        TransferAttemptStatus::Failed
    );
}
