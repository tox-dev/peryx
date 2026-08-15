use crate::meta::{
    BackpressureState, IntentAdmission, IntentLimits, IntentPhase, IntentStageOutcome, IntentStageResult,
    IntentTransition, IntentUsage, MetaStore, StagedIntent,
};

use super::store;

const LIMITS: IntentLimits = IntentLimits {
    max_records: 1_000,
    max_bytes: 1 << 30,
    backpressure_percent: 80,
};

fn limits(records: u64, bytes: u64) -> IntentLimits {
    IntentLimits {
        max_records: records,
        max_bytes: bytes,
        backpressure_percent: 80,
    }
}

fn adm<'a>(authority: &'a str, key: &'a str, digest: &'a str, size: u64, payload: &'a [u8]) -> IntentAdmission<'a> {
    IntentAdmission {
        authority,
        key,
        digest,
        size,
        payload,
    }
}

fn stage(store: &MetaStore, key: &str, digest: &str, size: u64, payload: &[u8], now: i64) -> IntentStageResult {
    store
        .stage_intent(adm("auth", key, digest, size, payload), LIMITS, now)
        .unwrap()
}

fn pending(seq: u64, digest: &str, size: u64, payload: &[u8], now: i64) -> StagedIntent {
    StagedIntent {
        phase: IntentPhase::Pending,
        authority: "auth".to_owned(),
        seq,
        digest: digest.to_owned(),
        size,
        payload: payload.to_vec(),
        updated_at_unix: now,
    }
}

#[test]
fn test_stage_admits_a_new_intent() {
    let (_dir, store) = store();

    assert_eq!(
        stage(&store, "key-1", "digest-a", 10, b"intent", 1),
        IntentStageResult {
            outcome: IntentStageOutcome::Admitted,
            pressure: BackpressureState::Nominal,
        }
    );
    assert_eq!(
        store.staged_intent("key-1").unwrap(),
        Some(pending(0, "digest-a", 10, b"intent", 1))
    );
    assert_eq!(store.count_staged_intents().unwrap(), 1);
    assert_eq!(
        store.staged_intent_usage("auth").unwrap(),
        IntentUsage { records: 1, bytes: 10 }
    );
}

#[test]
fn test_restaging_the_same_content_is_a_duplicate() {
    let (_dir, store) = store();
    stage(&store, "key-1", "digest-a", 10, b"first", 1);

    assert_eq!(
        stage(&store, "key-1", "digest-a", 10, b"second", 2),
        IntentStageResult {
            outcome: IntentStageOutcome::Duplicate,
            pressure: BackpressureState::Nominal,
        }
    );
    assert_eq!(
        store.staged_intent("key-1").unwrap(),
        Some(pending(0, "digest-a", 10, b"first", 1))
    );
    assert_eq!(store.count_staged_intents().unwrap(), 1);
    assert_eq!(
        store.staged_intent_usage("auth").unwrap(),
        IntentUsage { records: 1, bytes: 10 }
    );
}

#[test]
fn test_restaging_a_different_digest_is_a_conflict() {
    let (_dir, store) = store();
    stage(&store, "key-1", "digest-a", 10, b"first", 1);

    assert_eq!(
        stage(&store, "key-1", "digest-b", 10, b"second", 2),
        IntentStageResult {
            outcome: IntentStageOutcome::Conflict,
            pressure: BackpressureState::Nominal,
        }
    );
    assert_eq!(
        store.staged_intent("key-1").unwrap(),
        Some(pending(0, "digest-a", 10, b"first", 1))
    );
}

#[test]
fn test_restaging_a_different_size_is_a_conflict() {
    let (_dir, store) = store();
    stage(&store, "key-1", "digest-a", 10, b"first", 1);

    assert_eq!(
        stage(&store, "key-1", "digest-a", 20, b"second", 2),
        IntentStageResult {
            outcome: IntentStageOutcome::Conflict,
            pressure: BackpressureState::Nominal,
        }
    );
}

#[test]
fn test_a_new_key_past_the_record_ceiling_is_rejected_but_a_duplicate_is_not() {
    let (_dir, store) = store();
    let bound = limits(1, 1 << 20);
    assert_eq!(
        store
            .stage_intent(adm("auth", "key-1", "digest-a", 10, b"one"), bound, 1)
            .unwrap(),
        IntentStageResult {
            outcome: IntentStageOutcome::Admitted,
            pressure: BackpressureState::Backpressured,
        }
    );

    assert_eq!(
        store
            .stage_intent(adm("auth", "key-2", "digest-b", 10, b"two"), bound, 2)
            .unwrap(),
        IntentStageResult {
            outcome: IntentStageOutcome::RejectedOverRecordLimit,
            pressure: BackpressureState::Backpressured,
        }
    );
    assert_eq!(
        store
            .stage_intent(adm("auth", "key-1", "digest-a", 10, b"one"), bound, 3)
            .unwrap(),
        IntentStageResult {
            outcome: IntentStageOutcome::Duplicate,
            pressure: BackpressureState::Backpressured,
        }
    );
}

#[test]
fn test_a_new_key_that_would_cross_the_byte_ceiling_is_rejected() {
    let (_dir, store) = store();
    let bound = limits(8, 15);
    assert_eq!(
        store
            .stage_intent(adm("auth", "key-1", "digest-a", 10, b"one"), bound, 1)
            .unwrap(),
        IntentStageResult {
            outcome: IntentStageOutcome::Admitted,
            pressure: BackpressureState::Nominal,
        }
    );

    assert_eq!(
        store
            .stage_intent(adm("auth", "key-2", "digest-b", 10, b"two"), bound, 2)
            .unwrap(),
        IntentStageResult {
            outcome: IntentStageOutcome::RejectedOverByteLimit,
            pressure: BackpressureState::Nominal,
        }
    );
    assert_eq!(
        store.staged_intent_usage("auth").unwrap(),
        IntentUsage { records: 1, bytes: 10 }
    );
}

#[test]
fn test_stage_reports_nominal_below_the_soft_threshold() {
    let (_dir, store) = store();
    let bound = limits(5, 1 << 20);
    store
        .stage_intent(adm("auth", "key-1", "d", 1, b"x"), bound, 1)
        .unwrap();
    store
        .stage_intent(adm("auth", "key-2", "d", 1, b"x"), bound, 1)
        .unwrap();

    let result = store
        .stage_intent(adm("auth", "key-3", "d", 1, b"x"), bound, 1)
        .unwrap();

    assert_eq!(
        result,
        IntentStageResult {
            outcome: IntentStageOutcome::Admitted,
            pressure: BackpressureState::Nominal,
        }
    );
}

#[test]
fn test_stage_trips_backpressure_at_the_soft_record_threshold() {
    let (_dir, store) = store();
    let bound = limits(5, 1 << 20);
    for key in ["key-1", "key-2", "key-3"] {
        store.stage_intent(adm("auth", key, "d", 1, b"x"), bound, 1).unwrap();
    }

    let result = store
        .stage_intent(adm("auth", "key-4", "d", 1, b"x"), bound, 1)
        .unwrap();

    assert_eq!(
        result,
        IntentStageResult {
            outcome: IntentStageOutcome::Admitted,
            pressure: BackpressureState::Backpressured,
        }
    );
}

#[test]
fn test_stage_trips_backpressure_at_the_soft_byte_threshold() {
    let (_dir, store) = store();
    let bound = limits(100, 10);

    let result = store
        .stage_intent(adm("auth", "key-1", "d", 8, b"x"), bound, 1)
        .unwrap();

    assert_eq!(
        result,
        IntentStageResult {
            outcome: IntentStageOutcome::Admitted,
            pressure: BackpressureState::Backpressured,
        }
    );
}

#[test]
fn test_usage_is_tracked_per_authority() {
    let (_dir, store) = store();
    store
        .stage_intent(adm("alpha", "a-1", "d", 3, b"x"), LIMITS, 1)
        .unwrap();
    store
        .stage_intent(adm("alpha", "a-2", "d", 4, b"x"), LIMITS, 1)
        .unwrap();
    store.stage_intent(adm("beta", "b-1", "d", 5, b"x"), LIMITS, 1).unwrap();

    assert_eq!(
        store.staged_intent_usage("alpha").unwrap(),
        IntentUsage { records: 2, bytes: 7 }
    );
    assert_eq!(
        store.staged_intent_usage("beta").unwrap(),
        IntentUsage { records: 1, bytes: 5 }
    );
    assert_eq!(store.staged_intent_usage("unknown").unwrap(), IntentUsage::default());
}

#[test]
fn test_advance_moves_the_phase_forward_and_keeps_the_slot() {
    let (_dir, store) = store();
    stage(&store, "key-1", "digest-a", 10, b"intent", 1);

    assert_eq!(
        store.advance_intent("key-1", IntentPhase::Admitted, 2).unwrap(),
        IntentTransition::Advanced
    );
    assert_eq!(
        store.staged_intent("key-1").unwrap(),
        Some(StagedIntent {
            phase: IntentPhase::Admitted,
            authority: "auth".to_owned(),
            seq: 0,
            digest: "digest-a".to_owned(),
            size: 10,
            payload: b"intent".to_vec(),
            updated_at_unix: 2,
        })
    );
    assert_eq!(
        store.staged_intent_usage("auth").unwrap(),
        IntentUsage { records: 1, bytes: 10 }
    );
}

#[test]
fn test_advance_to_expired() {
    let (_dir, store) = store();
    stage(&store, "key-1", "digest-a", 10, b"intent", 1);

    assert_eq!(
        store.advance_intent("key-1", IntentPhase::Expired, 9).unwrap(),
        IntentTransition::Advanced
    );
    assert_eq!(
        store.staged_intent("key-1").unwrap(),
        Some(StagedIntent {
            phase: IntentPhase::Expired,
            authority: "auth".to_owned(),
            seq: 0,
            digest: "digest-a".to_owned(),
            size: 10,
            payload: b"intent".to_vec(),
            updated_at_unix: 9,
        })
    );
}

#[test]
fn test_advance_ignores_a_backward_or_equal_transition() {
    let (_dir, store) = store();
    stage(&store, "key-1", "digest-a", 10, b"intent", 1);
    store.advance_intent("key-1", IntentPhase::Admitted, 2).unwrap();

    assert_eq!(
        store.advance_intent("key-1", IntentPhase::Pending, 3).unwrap(),
        IntentTransition::Ignored
    );
    assert_eq!(
        store.advance_intent("key-1", IntentPhase::Admitted, 4).unwrap(),
        IntentTransition::Ignored
    );
    assert_eq!(
        store.staged_intent("key-1").unwrap(),
        Some(StagedIntent {
            phase: IntentPhase::Admitted,
            authority: "auth".to_owned(),
            seq: 0,
            digest: "digest-a".to_owned(),
            size: 10,
            payload: b"intent".to_vec(),
            updated_at_unix: 2,
        })
    );
}

#[test]
fn test_advance_ignores_an_unknown_intent() {
    let (_dir, store) = store();

    assert_eq!(
        store.advance_intent("ghost", IntentPhase::Admitted, 1).unwrap(),
        IntentTransition::Ignored
    );
}

#[test]
fn test_staged_intent_is_none_for_an_unknown_key() {
    let (_dir, store) = store();
    assert_eq!(store.staged_intent("unknown").unwrap(), None);
    assert_eq!(store.count_staged_intents().unwrap(), 0);
}

#[test]
fn test_list_pending_intents_returns_only_pending_in_admission_order_bounded() {
    let (_dir, store) = store();
    for key in ["key-c", "key-a", "key-d", "key-b"] {
        stage(&store, key, "digest", 1, b"x", 1);
    }
    store.advance_intent("key-d", IntentPhase::Admitted, 2).unwrap();

    let records = store.list_pending_intents(2).unwrap();

    assert_eq!(
        records,
        vec![
            ("key-c".to_owned(), pending(0, "digest", 1, b"x", 1)),
            ("key-a".to_owned(), pending(1, "digest", 1, b"x", 1)),
        ]
    );
}

#[test]
fn test_list_pending_intents_is_empty_without_pending_work() {
    let (_dir, store) = store();
    stage(&store, "key-1", "digest", 1, b"x", 1);
    store.advance_intent("key-1", IntentPhase::Admitted, 2).unwrap();

    assert!(store.list_pending_intents(10).unwrap().is_empty());
}

#[test]
fn test_pending_order_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    {
        let store = MetaStore::open(&path).unwrap();
        store
            .stage_intent(adm("alpha", "key-c", "d", 1, b"x"), LIMITS, 1)
            .unwrap();
        store
            .stage_intent(adm("beta", "key-a", "d", 1, b"x"), LIMITS, 1)
            .unwrap();
        store
            .stage_intent(adm("alpha", "key-d", "d", 1, b"x"), LIMITS, 1)
            .unwrap();
        store
            .stage_intent(adm("beta", "key-b", "d", 1, b"x"), LIMITS, 1)
            .unwrap();
    }

    let store = MetaStore::open_existing(&path).unwrap();
    let pending = store.list_pending_intents(10).unwrap();

    assert_eq!(
        pending,
        vec![
            (
                "key-c".to_owned(),
                StagedIntent {
                    phase: IntentPhase::Pending,
                    authority: "alpha".to_owned(),
                    seq: 0,
                    digest: "d".to_owned(),
                    size: 1,
                    payload: b"x".to_vec(),
                    updated_at_unix: 1,
                },
            ),
            (
                "key-a".to_owned(),
                StagedIntent {
                    phase: IntentPhase::Pending,
                    authority: "beta".to_owned(),
                    seq: 1,
                    digest: "d".to_owned(),
                    size: 1,
                    payload: b"x".to_vec(),
                    updated_at_unix: 1,
                },
            ),
            (
                "key-d".to_owned(),
                StagedIntent {
                    phase: IntentPhase::Pending,
                    authority: "alpha".to_owned(),
                    seq: 2,
                    digest: "d".to_owned(),
                    size: 1,
                    payload: b"x".to_vec(),
                    updated_at_unix: 1,
                },
            ),
            (
                "key-b".to_owned(),
                StagedIntent {
                    phase: IntentPhase::Pending,
                    authority: "beta".to_owned(),
                    seq: 3,
                    digest: "d".to_owned(),
                    size: 1,
                    payload: b"x".to_vec(),
                    updated_at_unix: 1,
                },
            ),
        ]
    );
}

fn admit(store: &MetaStore, key: &str, digest: &str, staged_at: i64, admitted_at: i64) {
    store
        .stage_intent(adm("auth", key, digest, 10, b"intent"), LIMITS, staged_at)
        .unwrap();
    assert_eq!(
        store.advance_intent(key, IntentPhase::Admitted, admitted_at).unwrap(),
        IntentTransition::Advanced
    );
}

#[test]
fn test_prune_removes_admitted_intents_past_retention() {
    let (_dir, store) = store();
    admit(&store, "key-1", "digest-a", 1, 10);

    assert_eq!(store.prune_ingress_intents(70, 60, 100).unwrap(), 1);
    assert_eq!(store.staged_intent("key-1").unwrap(), None);
    assert_eq!(store.count_staged_intents().unwrap(), 0);
}

#[test]
fn test_prune_releases_the_authority_usage_and_reuses_no_sequence() {
    let (_dir, store) = store();
    admit(&store, "key-1", "digest-a", 1, 10);
    assert_eq!(
        store.staged_intent_usage("auth").unwrap(),
        IntentUsage { records: 1, bytes: 10 }
    );

    assert_eq!(store.prune_ingress_intents(70, 60, 100).unwrap(), 1);
    assert_eq!(store.staged_intent_usage("auth").unwrap(), IntentUsage::default());
    assert!(store.list_pending_intents(10).unwrap().is_empty());

    stage(&store, "key-2", "digest-b", 3, b"x", 80);
    assert_eq!(
        store.staged_intent("key-2").unwrap(),
        Some(pending(1, "digest-b", 3, b"x", 80))
    );
}

#[test]
fn test_prune_removes_expired_intents_past_retention() {
    let (_dir, store) = store();
    store
        .stage_intent(adm("auth", "key-1", "digest-a", 10, b"intent"), LIMITS, 1)
        .unwrap();
    store.advance_intent("key-1", IntentPhase::Expired, 10).unwrap();

    assert_eq!(store.prune_ingress_intents(70, 60, 100).unwrap(), 1);
    assert_eq!(store.staged_intent("key-1").unwrap(), None);
    assert_eq!(store.staged_intent_usage("auth").unwrap(), IntentUsage::default());
}

#[test]
fn test_prune_keeps_a_settled_intent_within_retention() {
    let (_dir, store) = store();
    admit(&store, "key-1", "digest-a", 1, 10);

    assert_eq!(store.prune_ingress_intents(69, 60, 100).unwrap(), 0);
    assert_eq!(
        store.staged_intent("key-1").unwrap(),
        Some(StagedIntent {
            phase: IntentPhase::Admitted,
            authority: "auth".to_owned(),
            seq: 0,
            digest: "digest-a".to_owned(),
            size: 10,
            payload: b"intent".to_vec(),
            updated_at_unix: 10,
        })
    );
}

#[test]
fn test_prune_never_removes_a_pending_intent() {
    let (_dir, store) = store();
    store
        .stage_intent(adm("auth", "key-1", "digest-a", 10, b"intent"), LIMITS, 1)
        .unwrap();

    assert_eq!(store.prune_ingress_intents(1_000_000, 60, 100).unwrap(), 0);
    assert_eq!(
        store.staged_intent("key-1").unwrap(),
        Some(pending(0, "digest-a", 10, b"intent", 1))
    );
}

#[test]
fn test_prune_honors_the_batch_limit() {
    let (_dir, store) = store();
    admit(&store, "key-1", "digest-a", 1, 10);
    admit(&store, "key-2", "digest-b", 1, 10);
    admit(&store, "key-3", "digest-c", 1, 10);

    assert_eq!(store.prune_ingress_intents(70, 60, 2).unwrap(), 2);
    assert_eq!(store.count_staged_intents().unwrap(), 1);
}

#[test]
fn test_prune_keeps_one_authority_usage_when_another_slot_remains() {
    let (_dir, store) = store();
    admit(&store, "key-1", "digest-a", 1, 10);
    admit(&store, "key-2", "digest-b", 1, 10);

    assert_eq!(store.prune_ingress_intents(70, 60, 1).unwrap(), 1);
    assert_eq!(
        store.staged_intent_usage("auth").unwrap(),
        IntentUsage { records: 1, bytes: 10 }
    );
}
