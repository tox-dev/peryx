use rstest::rstest;

use crate::meta::{IntentAdmission, IntentLimits, IntentPhase, IntentUpdate, IntentUsage, MetaStore, StagedIntent};

use super::store;

const LIMITS: IntentLimits = IntentLimits {
    max_records: 1_000,
    max_bytes: 1 << 30,
    backpressure_percent: 80,
};

const STAGED_AT: i64 = 100;
const DEADLINE: i64 = 60;

fn stage(store: &MetaStore, key: &str) {
    store
        .stage_intent(
            IntentAdmission {
                authority: "auth",
                key,
                digest: "digest-a",
                size: 10,
                payload: b"intent",
            },
            LIMITS,
            STAGED_AT,
        )
        .unwrap();
}

fn refuse(store: &MetaStore, key: &str, times: u32) {
    for _ in 0..times {
        assert_eq!(store.refuse_intent(key).unwrap(), IntentUpdate::Applied);
    }
}

fn record(seq: u64, phase: IntentPhase, refusals: u32, updated_at_unix: i64) -> StagedIntent {
    StagedIntent {
        phase,
        authority: "auth".to_owned(),
        seq,
        digest: "digest-a".to_owned(),
        size: 10,
        payload: b"intent".to_vec(),
        refusals,
        updated_at_unix,
    }
}

#[test]
fn test_refusals_accumulate_without_restarting_the_staging_deadline() {
    let (_dir, store) = store();
    stage(&store, "key-1");

    refuse(&store, "key-1", 2);

    assert_eq!(
        store.staged_intent("key-1").unwrap(),
        Some(record(0, IntentPhase::Pending, 2, STAGED_AT))
    );
}

#[rstest]
#[case::settled(true)]
#[case::missing(false)]
fn test_refusing_an_intent_that_is_not_pending_is_ignored(#[case] settled: bool) {
    let (_dir, store) = store();
    if settled {
        stage(&store, "key-1");
        store.advance_intent("key-1", IntentPhase::Admitted, 200).unwrap();
    }

    assert_eq!(store.refuse_intent("key-1").unwrap(), IntentUpdate::Ignored);
}

#[test]
fn test_releasing_a_pending_intent_returns_its_authority_capacity() {
    let (_dir, store) = store();
    stage(&store, "key-1");
    stage(&store, "key-2");

    assert_eq!(store.release_intent("key-1").unwrap(), IntentUpdate::Applied);

    assert_eq!(store.staged_intent("key-1").unwrap(), None);
    assert_eq!(
        store.staged_intent_usage("auth").unwrap(),
        IntentUsage { records: 1, bytes: 10 }
    );
    assert_eq!(
        store
            .list_pending_intents(10, u32::MAX)
            .unwrap()
            .iter()
            .map(|(key, _)| key.as_str())
            .collect::<Vec<_>>(),
        ["key-2"],
        "the released intent leaves the pending order behind it intact"
    );
}

#[test]
fn test_releasing_the_last_intent_drops_the_authority_counter() {
    let (_dir, store) = store();
    stage(&store, "key-1");

    assert_eq!(store.release_intent("key-1").unwrap(), IntentUpdate::Applied);

    assert_eq!(store.staged_intent_usage("auth").unwrap(), IntentUsage::default());
}

#[rstest]
#[case::settled(true)]
#[case::missing(false)]
fn test_releasing_an_intent_that_is_not_pending_retains_it(#[case] settled: bool) {
    let (_dir, store) = store();
    if settled {
        stage(&store, "key-1");
        store.advance_intent("key-1", IntentPhase::Admitted, 200).unwrap();
    }

    assert_eq!(store.release_intent("key-1").unwrap(), IntentUpdate::Ignored);

    assert_eq!(
        store.staged_intent("key-1").unwrap(),
        settled.then(|| record(0, IntentPhase::Admitted, 0, 200))
    );
}

#[test]
fn test_expiry_settles_a_refused_intent_past_its_deadline() {
    let (_dir, store) = store();
    stage(&store, "key-1");
    refuse(&store, "key-1", 2);

    assert_eq!(
        store
            .expire_stale_intents(STAGED_AT + DEADLINE, DEADLINE, 2, 10)
            .unwrap(),
        1
    );

    assert_eq!(
        store.staged_intent("key-1").unwrap(),
        Some(record(0, IntentPhase::Expired, 2, STAGED_AT + DEADLINE))
    );
}

#[test]
fn test_an_expired_intent_is_pruned_back_to_free_capacity() {
    let (_dir, store) = store();
    stage(&store, "key-1");
    refuse(&store, "key-1", 2);
    let expired_at = STAGED_AT + DEADLINE;
    store.expire_stale_intents(expired_at, DEADLINE, 2, 10).unwrap();

    assert_eq!(store.prune_ingress_intents(expired_at + 60, 60, 10).unwrap(), 1);

    assert_eq!(store.staged_intent("key-1").unwrap(), None);
    assert_eq!(store.staged_intent_usage("auth").unwrap(), IntentUsage::default());
}

#[rstest]
#[case::within_the_deadline(STAGED_AT + DEADLINE - 1, 2)]
#[case::not_refused_often_enough(STAGED_AT + DEADLINE, 3)]
fn test_expiry_leaves_an_intent_a_later_pass_could_still_finalize(#[case] now: i64, #[case] min_refusals: u32) {
    let (_dir, store) = store();
    stage(&store, "key-1");
    refuse(&store, "key-1", 2);

    assert_eq!(store.expire_stale_intents(now, DEADLINE, min_refusals, 10).unwrap(), 0);

    assert_eq!(
        store.staged_intent("key-1").unwrap(),
        Some(record(0, IntentPhase::Pending, 2, STAGED_AT))
    );
}

#[test]
fn test_expiry_never_settles_an_intent_no_sweep_has_refused() {
    let (_dir, store) = store();
    stage(&store, "key-1");

    assert_eq!(
        store
            .expire_stale_intents(STAGED_AT + 1_000_000, DEADLINE, 1, 10)
            .unwrap(),
        0,
        "age alone never expires an intent a slow home datacenter may still finalize"
    );
}

#[test]
fn test_expiry_honors_the_batch_limit() {
    let (_dir, store) = store();
    for key in ["key-1", "key-2", "key-3"] {
        stage(&store, key);
        refuse(&store, key, 1);
    }

    assert_eq!(
        store
            .expire_stale_intents(STAGED_AT + DEADLINE, DEADLINE, 1, 2)
            .unwrap(),
        2
    );

    assert_eq!(store.list_pending_intents(10, u32::MAX).unwrap().len(), 1);
}

#[test]
fn test_listing_skips_the_intents_a_sweep_has_given_up_on() {
    let (_dir, store) = store();
    stage(&store, "key-1");
    stage(&store, "key-2");
    refuse(&store, "key-1", 2);

    let offered = store.list_pending_intents(10, 2).unwrap();

    assert_eq!(
        offered,
        vec![("key-2".to_owned(), record(1, IntentPhase::Pending, 0, STAGED_AT))],
        "a head of intents no upload can finalize does not fill the batch"
    );
}
