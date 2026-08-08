use crate::meta::{
    BackpressureState, IntentAdmission, IntentLimits, IntentPhase, IntentStageOutcome, IntentStageResult,
    IntentTransition, IntentUsage, MetaStore, StagedIntent,
};

/// Generous ceilings for the tests that exercise a single admission rather than the bounds themselves.
const LIMITS: IntentLimits = IntentLimits {
    max_records: 1_000,
    max_bytes: 1 << 30,
    backpressure_percent: 80,
};

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, store)
}

/// Per-authority ceilings for the tests that exercise the bounds, keeping the production 80% backpressure
/// fraction.
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

/// Stage under authority `auth` with generous ceilings, returning the outcome most tests assert on.
fn stage(store: &MetaStore, key: &str, digest: &str, size: u64, payload: &[u8], now: i64) -> IntentStageOutcome {
    store
        .stage_intent(adm("auth", key, digest, size, payload), LIMITS, now)
        .unwrap()
        .outcome
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
        IntentStageOutcome::Admitted
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
        IntentStageOutcome::Duplicate
    );
    // The first admission stands: neither the payload, the count, nor the usage changed.
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
        IntentStageOutcome::Conflict
    );
    assert_eq!(store.staged_intent("key-1").unwrap().unwrap().digest, "digest-a");
}

#[test]
fn test_restaging_a_different_size_is_a_conflict() {
    let (_dir, store) = store();
    stage(&store, "key-1", "digest-a", 10, b"first", 1);

    assert_eq!(
        stage(&store, "key-1", "digest-a", 20, b"second", 2),
        IntentStageOutcome::Conflict
    );
}

#[test]
fn test_a_new_key_past_the_record_ceiling_is_rejected_but_a_duplicate_is_not() {
    let (_dir, store) = store();
    let bound = limits(1, 1 << 20);
    assert_eq!(
        store
            .stage_intent(adm("auth", "key-1", "digest-a", 10, b"one"), bound, 1)
            .unwrap()
            .outcome,
        IntentStageOutcome::Admitted
    );

    assert_eq!(
        store
            .stage_intent(adm("auth", "key-2", "digest-b", 10, b"two"), bound, 2)
            .unwrap()
            .outcome,
        IntentStageOutcome::RejectedOverRecordLimit
    );
    // An existing key is deduplicated before the ceiling, so a retry still resolves past a full buffer.
    assert_eq!(
        store
            .stage_intent(adm("auth", "key-1", "digest-a", 10, b"one"), bound, 3)
            .unwrap()
            .outcome,
        IntentStageOutcome::Duplicate
    );
}

#[test]
fn test_a_new_key_that_would_cross_the_byte_ceiling_is_rejected() {
    let (_dir, store) = store();
    let bound = limits(8, 15);
    assert_eq!(
        store
            .stage_intent(adm("auth", "key-1", "digest-a", 10, b"one"), bound, 1)
            .unwrap()
            .outcome,
        IntentStageOutcome::Admitted
    );

    // A second 10-byte intent would push the authority to 20 bytes, past its 15-byte ceiling.
    assert_eq!(
        store
            .stage_intent(adm("auth", "key-2", "digest-b", 10, b"two"), bound, 2)
            .unwrap()
            .outcome,
        IntentStageOutcome::RejectedOverByteLimit
    );
    assert_eq!(
        store.staged_intent_usage("auth").unwrap(),
        IntentUsage { records: 1, bytes: 10 }
    );
}

#[test]
fn test_stage_reports_nominal_below_the_soft_threshold() {
    let (_dir, store) = store();
    // Five records, backpressure at 80%: the soft threshold is four, so three retained sits below it.
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
    // The fourth retained record reaches the soft threshold of four, one shed signal ahead of the ceiling.
    let bound = limits(5, 1 << 20);
    for key in ["key-1", "key-2", "key-3"] {
        store.stage_intent(adm("auth", key, "d", 1, b"x"), bound, 1).unwrap();
    }

    let result = store
        .stage_intent(adm("auth", "key-4", "d", 1, b"x"), bound, 1)
        .unwrap();

    assert_eq!(result.outcome, IntentStageOutcome::Admitted);
    assert_eq!(result.pressure, BackpressureState::Backpressured);
}

#[test]
fn test_stage_trips_backpressure_at_the_soft_byte_threshold() {
    let (_dir, store) = store();
    // Ten-byte ceiling, soft threshold eight: an eight-byte intent reaches it though a record slot is free.
    let bound = limits(100, 10);

    let result = store
        .stage_intent(adm("auth", "key-1", "d", 8, b"x"), bound, 1)
        .unwrap();

    assert_eq!(result.outcome, IntentStageOutcome::Admitted);
    assert_eq!(result.pressure, BackpressureState::Backpressured);
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
    let record = store.staged_intent("key-1").unwrap().unwrap();
    assert_eq!(record.phase, IntentPhase::Admitted);
    assert_eq!(record.updated_at_unix, 2);
    // A settled intent still occupies its slot until the reaper prunes it.
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
        store.staged_intent("key-1").unwrap().unwrap().phase,
        IntentPhase::Expired
    );
}

#[test]
fn test_advance_ignores_a_backward_or_equal_transition() {
    let (_dir, store) = store();
    stage(&store, "key-1", "digest-a", 10, b"intent", 1);
    store.advance_intent("key-1", IntentPhase::Admitted, 2).unwrap();

    // Backward: Admitted cannot drop to Pending.
    assert_eq!(
        store.advance_intent("key-1", IntentPhase::Pending, 3).unwrap(),
        IntentTransition::Ignored
    );
    // Equal: re-applying the current phase is a no-op.
    assert_eq!(
        store.advance_intent("key-1", IntentPhase::Admitted, 4).unwrap(),
        IntentTransition::Ignored
    );
    assert_eq!(
        store.staged_intent("key-1").unwrap().unwrap().phase,
        IntentPhase::Admitted
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
    // Staged out of key order: the drain resumes in admission order, not the key order the table iterates.
    for key in ["key-c", "key-a", "key-d", "key-b"] {
        stage(&store, key, "digest", 1, b"x", 1);
    }
    // Advancing one out of Pending drops it from the drain's work set though its order entry lingers.
    store.advance_intent("key-d", IntentPhase::Admitted, 2).unwrap();

    let pending = store.list_pending_intents(2).unwrap();

    assert_eq!(
        pending.iter().map(|(key, _)| key.as_str()).collect::<Vec<_>>(),
        ["key-c", "key-a"]
    );
    assert!(pending.iter().all(|(_, record)| record.phase == IntentPhase::Pending));
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
        // Interleaved authorities, staged out of key order: admission order is c, a, d, b.
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
        pending
            .iter()
            .map(|(key, record)| (key.as_str(), record.seq))
            .collect::<Vec<_>>(),
        [("key-c", 0), ("key-a", 1), ("key-d", 2), ("key-b", 3)]
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

    // At 70 with a 60s retention the intent settled at 10 is eligible, so it is reaped.
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
    // The pruned slot returns to the authority, and the order entry is gone.
    assert_eq!(store.staged_intent_usage("auth").unwrap(), IntentUsage::default());
    assert!(store.list_pending_intents(10).unwrap().is_empty());

    // A fresh admission draws the next sequence rather than reusing the pruned one.
    stage(&store, "key-2", "digest-b", 3, b"x", 80);
    assert_eq!(store.staged_intent("key-2").unwrap().unwrap().seq, 1);
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

    // At 69 the 60s window since the transition at 10 has not elapsed, so the intent stays.
    assert_eq!(store.prune_ingress_intents(69, 60, 100).unwrap(), 0);
    assert_eq!(
        store.staged_intent("key-1").unwrap().unwrap().phase,
        IntentPhase::Admitted
    );
}

#[test]
fn test_prune_never_removes_a_pending_intent() {
    let (_dir, store) = store();
    // A pending intent staged long ago is never reaped: its write may still finalize.
    store
        .stage_intent(adm("auth", "key-1", "digest-a", 10, b"intent"), LIMITS, 1)
        .unwrap();

    assert_eq!(store.prune_ingress_intents(1_000_000, 60, 100).unwrap(), 0);
    assert_eq!(
        store.staged_intent("key-1").unwrap().unwrap().phase,
        IntentPhase::Pending
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

    // One of the authority's two settled slots is reaped, so its counter is decremented, not cleared.
    assert_eq!(store.prune_ingress_intents(70, 60, 1).unwrap(), 1);
    assert_eq!(
        store.staged_intent_usage("auth").unwrap(),
        IntentUsage { records: 1, bytes: 10 }
    );
}
