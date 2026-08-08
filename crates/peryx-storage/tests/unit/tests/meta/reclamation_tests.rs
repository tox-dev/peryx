use std::str::FromStr as _;

use peryx_identity::ArtifactDigest;
use rstest::rstest;
use tempfile::TempDir;

use crate::meta::{
    BackendId, BackendLocation, BlobPlacementKey, BlobPlacementTransition, DataCenterId, MetaStore, ObservedFrontier,
    ReadyOutcome, ReclamationError, ReclamationProgress, ReclamationState, ReclamationTombstone, SelectOutcome,
    SkipReason,
};

fn store() -> (TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, store)
}

fn digest(suffix: u8) -> ArtifactDigest {
    ArtifactDigest::from_str(&format!("sha256:{suffix:064x}")).unwrap()
}

fn frontier(replica: u64, backup: u64) -> ObservedFrontier {
    ObservedFrontier {
        replica: Some(replica),
        backup: Some(backup),
    }
}

fn placement_key(suffix: u8) -> BlobPlacementKey {
    BlobPlacementKey {
        digest: digest(suffix),
        backend: BackendId::new("filesystem").unwrap(),
        data_center: DataCenterId::new("dc-1").unwrap(),
        location: BackendLocation::new("blobs/aa").unwrap(),
    }
}

fn stage_placement(store: &MetaStore, suffix: u8) {
    store
        .apply_blob_placement(&placement_key(suffix), &BlobPlacementTransition::Stage, 1, 0)
        .unwrap();
}

fn make_serveable(store: &MetaStore, suffix: u8) {
    stage_placement(store, suffix);
    store
        .apply_blob_placement(
            &placement_key(suffix),
            &BlobPlacementTransition::Verify {
                observed: digest(suffix),
                size: 10,
            },
            1,
            0,
        )
        .unwrap();
}

fn tombstone(store: &MetaStore, suffix: u8) -> ReclamationTombstone {
    store.reclamation_tombstone(&digest(suffix)).unwrap().unwrap()
}

#[rstest]
#[case::both_cover(5, 5, true)]
#[case::replica_short(4, 9, false)]
#[case::backup_short(9, 4, false)]
#[case::both_short(4, 4, false)]
fn test_observed_frontier_covers_requires_both_planes(
    #[case] replica: u64,
    #[case] backup: u64,
    #[case] covered: bool,
) {
    assert_eq!(frontier(replica, backup).covers(5), covered);
}

#[rstest]
#[case::none(None, None, true)]
#[case::replica_absent(None, Some(4), false)]
#[case::backup_absent(Some(4), None, false)]
fn test_observed_frontier_ignores_only_absent_planes(
    #[case] replica: Option<u64>,
    #[case] backup: Option<u64>,
    #[case] covered: bool,
) {
    assert_eq!(ObservedFrontier { replica, backup }.covers(5), covered);
}

#[test]
fn test_select_arms_a_pending_tombstone_for_an_eligible_digest() {
    let (_dir, store) = store();
    let outcome = store
        .select_reclamation_candidate(&digest(1), false, 7, 3, 100)
        .unwrap();
    let SelectOutcome::Selected(record) = outcome else {
        panic!("an eligible digest is selected, got {outcome:?}");
    };
    assert_eq!(record.state, ReclamationState::Pending);
    assert_eq!(record.required_frontier, 7);
    assert_eq!(record.fence, 3);
    assert_eq!(record.attempts, 1);
    assert_eq!(record.selected_at_unix, 100);
    assert_eq!(record.updated_at_unix, 100);
}

#[test]
fn test_select_ignores_a_pending_placement_that_cannot_serve() {
    let (_dir, store) = store();
    stage_placement(&store, 1);
    let outcome = store.select_reclamation_candidate(&digest(1), false, 1, 1, 0).unwrap();
    assert!(matches!(outcome, SelectOutcome::Selected(_)));
}

#[rstest]
#[case::referenced(true, false, SkipReason::Referenced)]
#[case::serveable(false, true, SkipReason::Serveable)]
fn test_select_ineligible_digest_persists_nothing(
    #[case] referenced: bool,
    #[case] serveable: bool,
    #[case] reason: SkipReason,
) {
    let (_dir, store) = store();
    if serveable {
        make_serveable(&store, 1);
    }
    let outcome = store
        .select_reclamation_candidate(&digest(1), referenced, 1, 1, 0)
        .unwrap();
    assert_eq!(outcome, SelectOutcome::Ineligible(reason));
    assert!(store.reclamation_tombstone(&digest(1)).unwrap().is_none());
}

#[test]
fn test_select_abandons_an_existing_tombstone_when_a_reference_reappears() {
    let (_dir, store) = store();
    store.select_reclamation_candidate(&digest(1), false, 1, 1, 0).unwrap();
    let outcome = store.select_reclamation_candidate(&digest(1), true, 1, 1, 5).unwrap();
    let SelectOutcome::Skipped(record) = outcome else {
        panic!("a reappearing reference abandons the tombstone, got {outcome:?}");
    };
    assert_eq!(
        record.state,
        ReclamationState::Skipped {
            reason: SkipReason::Referenced
        }
    );
    assert_eq!(tombstone(&store, 1).state, record.state);
}

#[test]
fn test_select_abandons_an_existing_tombstone_when_a_placement_becomes_serveable() {
    let (_dir, store) = store();
    store.select_reclamation_candidate(&digest(1), false, 1, 1, 0).unwrap();
    make_serveable(&store, 1);
    let outcome = store.select_reclamation_candidate(&digest(1), false, 1, 1, 5).unwrap();
    assert_eq!(
        tombstone(&store, 1).state,
        ReclamationState::Skipped {
            reason: SkipReason::Serveable
        }
    );
    assert!(matches!(outcome, SelectOutcome::Skipped(_)));
}

#[test]
fn test_select_raises_the_required_frontier_and_preserves_selection_time() {
    let (_dir, store) = store();
    store
        .select_reclamation_candidate(&digest(1), false, 9, 1, 100)
        .unwrap();
    let outcome = store
        .select_reclamation_candidate(&digest(1), false, 4, 1, 200)
        .unwrap();
    let SelectOutcome::Selected(record) = outcome else {
        panic!("a re-selection stays selected, got {outcome:?}");
    };
    assert_eq!(record.required_frontier, 9);
    assert_eq!(record.attempts, 2);
    assert_eq!(record.selected_at_unix, 100);
    assert_eq!(record.updated_at_unix, 200);
}

#[test]
fn test_select_rejects_a_stale_fence() {
    let (_dir, store) = store();
    store.select_reclamation_candidate(&digest(1), false, 1, 5, 0).unwrap();
    let error = store
        .select_reclamation_candidate(&digest(1), false, 1, 3, 1)
        .unwrap_err();
    assert!(matches!(error, ReclamationError::StaleFence { current: 5, applied: 3 }));
}

#[test]
fn test_select_rearms_a_ready_tombstone_back_to_pending() {
    let (_dir, store) = store();
    store.select_reclamation_candidate(&digest(1), false, 5, 1, 0).unwrap();
    store
        .mark_reclamation_ready(&digest(1), false, frontier(5, 5), 1, 1)
        .unwrap();
    assert_eq!(tombstone(&store, 1).state, ReclamationState::Ready);
    store.select_reclamation_candidate(&digest(1), false, 5, 1, 2).unwrap();
    assert_eq!(tombstone(&store, 1).state, ReclamationState::Pending);
}

#[test]
fn test_mark_ready_promotes_a_covered_unreferenced_candidate() {
    let (_dir, store) = store();
    store.select_reclamation_candidate(&digest(1), false, 5, 1, 0).unwrap();
    let outcome = store
        .mark_reclamation_ready(&digest(1), false, frontier(5, 6), 1, 10)
        .unwrap();
    let ReadyOutcome::Ready(record) = outcome else {
        panic!("a covered candidate is ready, got {outcome:?}");
    };
    assert_eq!(record.state, ReclamationState::Ready);
    assert_eq!(record.updated_at_unix, 10);
}

#[rstest]
#[case::replica_short(4, 9)]
#[case::backup_short(9, 4)]
fn test_mark_ready_leaves_a_lagging_candidate_pending(#[case] replica: u64, #[case] backup: u64) {
    let (_dir, store) = store();
    store.select_reclamation_candidate(&digest(1), false, 5, 1, 0).unwrap();
    let observed = frontier(replica, backup);
    let outcome = store
        .mark_reclamation_ready(&digest(1), false, observed, 1, 10)
        .unwrap();
    assert_eq!(
        outcome,
        ReadyOutcome::NotReady {
            tombstone: tombstone(&store, 1),
            observed,
        }
    );
    assert_eq!(tombstone(&store, 1).state, ReclamationState::Pending);
}

#[test]
fn test_mark_ready_skips_a_candidate_whose_reference_reappeared() {
    let (_dir, store) = store();
    store.select_reclamation_candidate(&digest(1), false, 5, 1, 0).unwrap();
    let outcome = store
        .mark_reclamation_ready(&digest(1), true, frontier(5, 5), 1, 10)
        .unwrap();
    let ReadyOutcome::Skipped(record) = outcome else {
        panic!("a reappearing reference skips the candidate, got {outcome:?}");
    };
    assert_eq!(
        record.state,
        ReclamationState::Skipped {
            reason: SkipReason::Referenced
        }
    );
}

#[test]
fn test_mark_ready_skips_a_candidate_that_became_serveable_again() {
    let (_dir, store) = store();
    store.select_reclamation_candidate(&digest(1), false, 5, 1, 0).unwrap();
    make_serveable(&store, 1);
    let outcome = store
        .mark_reclamation_ready(&digest(1), false, frontier(5, 5), 1, 10)
        .unwrap();
    assert_eq!(
        tombstone(&store, 1).state,
        ReclamationState::Skipped {
            reason: SkipReason::Serveable
        }
    );
    assert!(matches!(outcome, ReadyOutcome::Skipped(_)));
}

#[test]
fn test_mark_ready_without_a_candidate_is_missing() {
    let (_dir, store) = store();
    let error = store
        .mark_reclamation_ready(&digest(1), false, frontier(5, 5), 1, 0)
        .unwrap_err();
    assert!(matches!(error, ReclamationError::MissingCandidate));
}

#[test]
fn test_mark_ready_is_idempotent_once_ready() {
    let (_dir, store) = store();
    store.select_reclamation_candidate(&digest(1), false, 5, 1, 0).unwrap();
    let first = store
        .mark_reclamation_ready(&digest(1), false, frontier(5, 5), 1, 10)
        .unwrap();
    let again = store
        .mark_reclamation_ready(&digest(1), false, frontier(5, 5), 1, 20)
        .unwrap();
    assert_eq!(first, again);
}

#[test]
fn test_mark_ready_on_a_skipped_candidate_returns_skipped_unchanged() {
    let (_dir, store) = store();
    store.select_reclamation_candidate(&digest(1), false, 5, 1, 0).unwrap();
    store
        .mark_reclamation_ready(&digest(1), true, frontier(5, 5), 1, 10)
        .unwrap();
    let skipped = tombstone(&store, 1);
    let outcome = store
        .mark_reclamation_ready(&digest(1), false, frontier(5, 5), 1, 20)
        .unwrap();
    assert_eq!(outcome, ReadyOutcome::Skipped(skipped));
}

#[test]
fn test_a_stale_epoch_worker_cannot_mark_a_candidate_a_newer_epoch_owns() {
    let (_dir, store) = store();
    store.select_reclamation_candidate(&digest(1), false, 5, 5, 0).unwrap();
    store
        .mark_reclamation_ready(&digest(1), false, frontier(5, 5), 7, 10)
        .unwrap();
    assert_eq!(tombstone(&store, 1).fence, 7);
    let error = store
        .mark_reclamation_ready(&digest(1), false, frontier(5, 5), 5, 20)
        .unwrap_err();
    assert!(matches!(error, ReclamationError::StaleFence { current: 7, applied: 5 }));
    assert_eq!(tombstone(&store, 1).state, ReclamationState::Ready);
}

#[test]
fn test_forget_removes_a_tombstone_and_reports_it() {
    let (_dir, store) = store();
    store.select_reclamation_candidate(&digest(1), false, 5, 1, 0).unwrap();
    assert!(store.forget_reclamation_tombstone(&digest(1), 1).unwrap());
    assert!(store.reclamation_tombstone(&digest(1)).unwrap().is_none());
}

#[test]
fn test_forget_an_absent_tombstone_reports_false() {
    let (_dir, store) = store();
    assert!(!store.forget_reclamation_tombstone(&digest(1), 1).unwrap());
}

#[test]
fn test_forget_rejects_a_stale_fence() {
    let (_dir, store) = store();
    store.select_reclamation_candidate(&digest(1), false, 5, 5, 0).unwrap();
    let error = store.forget_reclamation_tombstone(&digest(1), 3).unwrap_err();
    assert!(matches!(error, ReclamationError::StaleFence { current: 5, applied: 3 }));
    assert!(store.reclamation_tombstone(&digest(1)).unwrap().is_some());
}

#[test]
fn test_reclamation_tombstones_lists_in_digest_order() {
    let (_dir, store) = store();
    store.select_reclamation_candidate(&digest(3), false, 1, 1, 0).unwrap();
    store.select_reclamation_candidate(&digest(1), false, 1, 1, 0).unwrap();
    let digests: Vec<_> = store
        .reclamation_tombstones()
        .unwrap()
        .into_iter()
        .map(|record| record.digest)
        .collect();
    assert_eq!(digests, vec![digest(1), digest(3)]);
}

#[test]
fn test_reclamation_progress_counts_by_state() {
    let (_dir, store) = store();
    store.select_reclamation_candidate(&digest(1), false, 5, 1, 0).unwrap();
    store.select_reclamation_candidate(&digest(2), false, 5, 1, 0).unwrap();
    store
        .mark_reclamation_ready(&digest(2), false, frontier(5, 5), 1, 1)
        .unwrap();
    store.select_reclamation_candidate(&digest(3), false, 5, 1, 0).unwrap();
    store
        .mark_reclamation_ready(&digest(3), true, frontier(5, 5), 1, 1)
        .unwrap();
    assert_eq!(
        store.reclamation_progress().unwrap(),
        ReclamationProgress {
            pending: 1,
            ready: 1,
            skipped: 1,
        }
    );
}

#[test]
fn test_prune_removes_skipped_tombstones_up_to_the_limit() {
    let (_dir, store) = store();
    for suffix in 1..=3 {
        store
            .select_reclamation_candidate(&digest(suffix), false, 5, 1, 0)
            .unwrap();
        store
            .mark_reclamation_ready(&digest(suffix), true, frontier(5, 5), 1, 1)
            .unwrap();
    }
    assert_eq!(store.prune_skipped_reclamation_tombstones(2).unwrap(), 2);
    assert_eq!(store.reclamation_progress().unwrap().skipped, 1);
    assert_eq!(store.prune_skipped_reclamation_tombstones(2).unwrap(), 1);
    assert!(store.reclamation_tombstones().unwrap().is_empty());
}

#[test]
fn test_prune_leaves_pending_and_ready_tombstones() {
    let (_dir, store) = store();
    store.select_reclamation_candidate(&digest(1), false, 5, 1, 0).unwrap();
    store.select_reclamation_candidate(&digest(2), false, 5, 1, 0).unwrap();
    store
        .mark_reclamation_ready(&digest(2), false, frontier(5, 5), 1, 1)
        .unwrap();
    store.select_reclamation_candidate(&digest(3), false, 5, 1, 0).unwrap();
    store
        .mark_reclamation_ready(&digest(3), true, frontier(5, 5), 1, 1)
        .unwrap();
    assert_eq!(store.prune_skipped_reclamation_tombstones(10).unwrap(), 1);
    assert_eq!(
        store.reclamation_progress().unwrap(),
        ReclamationProgress {
            pending: 1,
            ready: 1,
            skipped: 0,
        }
    );
}

#[test]
fn test_tombstones_survive_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let ready;
    {
        let store = MetaStore::open(&path).unwrap();
        store.select_reclamation_candidate(&digest(1), false, 5, 2, 0).unwrap();
        store
            .mark_reclamation_ready(&digest(1), false, frontier(5, 5), 2, 1)
            .unwrap();
        ready = tombstone(&store, 1);
    }
    let store = MetaStore::open_existing(&path).unwrap();
    assert_eq!(store.reclamation_tombstone(&digest(1)).unwrap(), Some(ready));
}
