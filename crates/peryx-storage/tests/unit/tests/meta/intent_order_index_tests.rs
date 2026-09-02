//! The pending-order index must hold nothing but pending intents. Each sweep below poisons the rows a
//! correct scan never touches, so a scan whose cost tracked the settled history would fail to decode
//! rather than pass with the right page.

use std::path::Path;

use rstest::rstest;

use crate::meta::{
    FinalizedWrite, IntentAdmission, IntentLimits, IntentPhase, IntentUpdate, MetaError, MetaStore, StagedIntent,
};

use super::store;

/// Ceilings exceed these admissions because these tests do not exercise bounds.
const LIMITS: IntentLimits = IntentLimits {
    max_records: 1_000,
    max_bytes: 1 << 30,
    backpressure_percent: 80,
};

const INTENTS: redb::TableDefinition<'static, &str, &[u8]> = redb::TableDefinition::new("ingress_intent");
const ORDER: redb::TableDefinition<'static, u64, &str> = redb::TableDefinition::new("ingress_intent_order");

const STAGED_AT: i64 = 100;
const DEADLINE: i64 = 60;

fn stage(store: &MetaStore, key: &str) {
    stage_for(store, "auth", key);
}

fn stage_for(store: &MetaStore, authority: &str, key: &str) {
    store
        .stage_intent(
            IntentAdmission {
                authority,
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

fn pending(seq: u64, refusals: u32) -> StagedIntent {
    StagedIntent {
        phase: IntentPhase::Pending,
        authority: "auth".to_owned(),
        seq,
        digest: "digest-a".to_owned(),
        size: 10,
        payload: b"intent".to_vec(),
        refusals,
        updated_at_unix: STAGED_AT,
    }
}

/// Overwrites the named intent rows with bytes no decoder accepts. Reading one is then an observable
/// failure rather than invisible work, which is what turns "the page is right" into "the settled rows
/// were never visited".
fn poison(path: &Path, keys: &[&str]) {
    let database = redb::Database::open(path).unwrap();
    let txn = database.begin_write().unwrap();
    {
        let mut table = txn.open_table(INTENTS).unwrap();
        for key in keys {
            table.insert(*key, b"not json".as_slice()).unwrap();
        }
    }
    txn.commit().unwrap();
    drop(database);
}

fn add_order_entry(path: &Path, seq: u64, key: &str) {
    let database = redb::Database::open(path).unwrap();
    let txn = database.begin_write().unwrap();
    {
        let mut table = txn.open_table(ORDER).unwrap();
        table.insert(seq, key).unwrap();
    }
    txn.commit().unwrap();
    drop(database);
}

fn staged_pair(path: &Path) -> MetaStore {
    let store = MetaStore::open(path).unwrap();
    stage(&store, "settled");
    stage(&store, "live");
    store
}

#[rstest]
#[case::admitted(IntentPhase::Admitted)]
#[case::expired(IntentPhase::Expired)]
fn test_a_sweep_never_reads_an_intent_an_advance_settled(#[case] to: IntentPhase) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let store = staged_pair(&path);
    store.advance_intent("settled", to, 200).unwrap();
    drop(store);
    poison(&path, &["settled"]);

    let store = MetaStore::open_existing(&path).unwrap();

    assert_eq!(
        store.list_pending_intents(10, u32::MAX).unwrap(),
        vec![("live".to_owned(), pending(1, 0))]
    );
}

#[test]
fn test_a_sweep_never_reads_an_intent_the_expiry_reaper_settled() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let store = staged_pair(&path);
    assert_eq!(store.refuse_intent("settled").unwrap(), IntentUpdate::Applied);
    assert_eq!(
        store
            .expire_stale_intents(STAGED_AT + DEADLINE, DEADLINE, 1, 10)
            .unwrap(),
        1
    );
    drop(store);
    poison(&path, &["settled"]);

    let store = MetaStore::open_existing(&path).unwrap();

    assert_eq!(
        store.list_pending_intents(10, u32::MAX).unwrap(),
        vec![("live".to_owned(), pending(1, 0))]
    );
}

#[test]
fn test_a_sweep_never_reads_an_intent_a_finalized_write_settled() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let store = staged_pair(&path);
    store
        .commit_finalized_write(
            FinalizedWrite {
                operation: "op",
                intent_key: "settled",
                response: b"response",
                expiry_unix: None,
                now: 200,
            },
            |_| Ok::<_, MetaError>(Vec::new()),
        )
        .unwrap();
    drop(store);
    poison(&path, &["settled"]);

    let store = MetaStore::open_existing(&path).unwrap();

    assert_eq!(
        store.list_pending_intents(10, u32::MAX).unwrap(),
        vec![("live".to_owned(), pending(1, 0))]
    );
}

#[test]
fn test_a_sweep_still_offers_a_refused_pending_intent() {
    let (_dir, store) = store();
    stage(&store, "refused");

    assert_eq!(store.refuse_intent("refused").unwrap(), IntentUpdate::Applied);

    assert_eq!(
        store.list_pending_intents(10, 2).unwrap(),
        vec![("refused".to_owned(), pending(0, 1))],
        "a refusal is not a settlement, so the intent stays offered until it expires"
    );
}

#[test]
fn test_a_sweep_skips_an_order_entry_whose_row_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let store = MetaStore::open(&path).unwrap();
    stage(&store, "live");
    drop(store);
    add_order_entry(&path, 7, "ghost");

    let store = MetaStore::open_existing(&path).unwrap();

    assert_eq!(
        store.list_pending_intents(10, u32::MAX).unwrap(),
        vec![("live".to_owned(), pending(0, 0))]
    );
}

#[test]
fn test_a_drain_page_holds_only_the_authority_it_names() {
    let (_dir, store) = store();
    stage_for(&store, "other", "other-first");
    stage_for(&store, "auth", "mine");
    stage_for(&store, "other", "other-last");

    assert_eq!(
        store.list_pending_intents_for("auth", None, 10).unwrap(),
        vec![("mine".to_owned(), pending(1, 0))]
    );
}

#[test]
fn test_a_drain_page_counts_the_named_authority_rather_than_the_rows_ahead_of_it() {
    let (_dir, store) = store();
    for key in ["other-0", "other-1", "other-2"] {
        stage_for(&store, "other", key);
    }
    stage_for(&store, "auth", "mine-0");
    stage_for(&store, "auth", "mine-1");

    assert_eq!(
        store.list_pending_intents_for("auth", None, 2).unwrap(),
        vec![
            ("mine-0".to_owned(), pending(3, 0)),
            ("mine-1".to_owned(), pending(4, 0))
        ],
        "a page bounded before the filter would report the authority as drained"
    );
}

#[test]
fn test_a_drain_resumes_past_the_intent_it_last_offered() {
    let (_dir, store) = store();
    for key in ["first", "second", "third"] {
        stage(&store, key);
    }

    assert_eq!(
        store.list_pending_intents_for("auth", Some(0), 10).unwrap(),
        vec![
            ("second".to_owned(), pending(1, 0)),
            ("third".to_owned(), pending(2, 0))
        ]
    );
}

#[test]
fn test_a_drain_offers_an_intent_the_finalize_sweep_gave_up_on() {
    let (_dir, store) = store();
    stage(&store, "refused");
    for _ in 0..3 {
        assert_eq!(store.refuse_intent("refused").unwrap(), IntentUpdate::Applied);
    }

    assert_eq!(
        store.list_pending_intents_for("auth", None, 10).unwrap(),
        vec![("refused".to_owned(), pending(0, 3))],
        "an operator drain settles every intent its home owns, refused or not"
    );
}

#[test]
fn test_a_drain_page_of_no_rows_reads_nothing() {
    let (_dir, store) = store();
    stage(&store, "live");

    assert_eq!(store.list_pending_intents_for("auth", None, 0).unwrap(), Vec::new());
}

#[test]
fn test_a_drain_of_an_authority_that_staged_nothing_is_empty() {
    let (_dir, store) = store();
    stage(&store, "live");

    assert_eq!(store.list_pending_intents_for("other", None, 10).unwrap(), Vec::new());
}
