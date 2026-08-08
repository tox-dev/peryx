use crate::meta::{
    FinalizeOutcome, FinalizedWrite, IntentAdmission, IntentLimits, IntentPhase, MetaError, MetaStore, OperationState,
};

/// Generous per-authority ceilings; these tests exercise a single admission, not the bounds.
const LIMITS: IntentLimits = IntentLimits {
    max_records: 1_000,
    max_bytes: 1 << 30,
    backpressure_percent: 80,
};

fn write(response: &[u8], expiry_unix: Option<i64>, now: i64) -> FinalizedWrite<'_> {
    FinalizedWrite {
        operation: "op",
        intent_key: "intent",
        response,
        expiry_unix,
        now,
    }
}

fn decode_error() -> MetaError {
    MetaError::from(serde_json::from_str::<serde_json::Value>("{").unwrap_err())
}

fn staged(store: &MetaStore) {
    store
        .stage_intent(
            IntentAdmission {
                authority: "auth",
                key: "intent",
                digest: "digest",
                size: 8,
                payload: b"payload",
            },
            LIMITS,
            1,
        )
        .unwrap();
}

fn publish(store: &MetaStore, response: &[u8]) -> Result<FinalizeOutcome, MetaError> {
    store.commit_finalized_write(write(response, Some(100), 2), |driver| {
        driver.put("row", b"value")?;
        Ok::<_, MetaError>(vec![b"{\"action\":\"add\"}".to_vec()])
    })
}

#[test]
fn test_commit_finalized_write_publishes_rows_outcome_and_intent_advance() {
    let (_dir, store) = super::store();
    staged(&store);

    let outcome = publish(&store, b"response").unwrap();

    assert_eq!(outcome, FinalizeOutcome::Published);
    assert_eq!(
        store.get_driver_value("row").unwrap().as_deref(),
        Some(b"value".as_slice())
    );
    assert_eq!(
        store.current_serial().unwrap(),
        1,
        "the journal entry allocates one serial"
    );
    let record = store.operation_outcome("op").unwrap().unwrap();
    assert_eq!(record.state, OperationState::Published);
    assert_eq!(record.response, b"response");
    assert_eq!(record.expiry_unix, Some(100));
    assert_eq!(
        store.staged_intent("intent").unwrap().unwrap().phase,
        IntentPhase::Admitted,
        "the finalize advances the intent out of pending"
    );
}

#[test]
fn test_commit_finalized_write_replays_the_first_result_without_a_second_write() {
    let (_dir, store) = super::store();
    staged(&store);
    publish(&store, b"first").unwrap();

    let replay = store
        .commit_finalized_write(write(b"second", Some(200), 5), |driver| {
            driver.put("row", b"clobbered")?;
            Ok::<_, MetaError>(vec![b"{\"action\":\"add\"}".to_vec()])
        })
        .unwrap();

    assert_eq!(
        replay,
        FinalizeOutcome::Replayed(store.operation_outcome("op").unwrap().unwrap())
    );
    assert!(
        matches!(&replay, FinalizeOutcome::Replayed(record) if record.response == b"first"),
        "the replay returns the first attempt's response"
    );
    assert_eq!(
        store.get_driver_value("row").unwrap().as_deref(),
        Some(b"value".as_slice()),
        "the replayed body's staged row is discarded"
    );
    assert_eq!(
        store.current_serial().unwrap(),
        1,
        "the replay appends no second journal entry"
    );
}

#[test]
fn test_commit_finalized_write_rolls_back_when_the_body_errors() {
    let (_dir, store) = super::store();
    staged(&store);

    let result = store.commit_finalized_write(write(b"response", None, 2), |driver| {
        driver.put("row", b"value")?;
        Err::<Vec<Vec<u8>>, _>(decode_error())
    });

    assert!(result.is_err(), "the body's error propagates");
    assert!(store.get_driver_value("row").unwrap().is_none(), "no row is committed");
    assert!(
        store.operation_outcome("op").unwrap().is_none(),
        "no outcome is stamped"
    );
    assert_eq!(store.current_serial().unwrap(), 0);
    assert_eq!(
        store.staged_intent("intent").unwrap().unwrap().phase,
        IntentPhase::Pending,
        "a rejected finalize leaves the intent pending"
    );
}

#[test]
fn test_commit_finalized_write_advances_nothing_for_an_absent_intent() {
    let (_dir, store) = super::store();

    let outcome = publish(&store, b"response").unwrap();

    assert_eq!(outcome, FinalizeOutcome::Published);
    assert!(
        store.staged_intent("intent").unwrap().is_none(),
        "no intent is created for an absent key"
    );
    assert_eq!(
        store.operation_outcome("op").unwrap().unwrap().state,
        OperationState::Published
    );
}

#[test]
fn test_commit_finalized_write_leaves_an_already_admitted_intent() {
    let (_dir, store) = super::store();
    staged(&store);
    store.advance_intent("intent", IntentPhase::Admitted, 3).unwrap();

    let outcome = publish(&store, b"response").unwrap();

    assert_eq!(outcome, FinalizeOutcome::Published);
    let intent = store.staged_intent("intent").unwrap().unwrap();
    assert_eq!(intent.phase, IntentPhase::Admitted);
    assert_eq!(
        intent.updated_at_unix, 3,
        "the finalize does not rewrite a settled intent"
    );
}
