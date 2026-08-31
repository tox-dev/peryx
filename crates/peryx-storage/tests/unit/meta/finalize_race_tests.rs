use super::*;

fn store() -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    (dir, store)
}

fn write(response: &[u8], now: i64) -> FinalizedWrite<'_> {
    FinalizedWrite {
        operation: "op",
        intent_key: "",
        response,
        expiry_unix: Some(10),
        now,
    }
}

/// Rebuilds the retention race by hand: the reaper's write transaction cannot interleave with the
/// finalize transaction, only with the replay that follows it.
#[test]
fn test_race_replay_answers_after_retention_prunes_the_observed_outcome() {
    let (_dir, store) = store();
    store
        .commit_finalized_write(write(b"ack", 2), |driver| {
            driver.put("row", b"value")?;
            Ok::<_, MetaError>(vec![b"{\"action\":\"add\"}".to_vec()])
        })
        .unwrap();

    let txn = store.db.begin_write().unwrap();
    let flow = stamp_finalized::<MetaError>(&txn, &write(b"retry", 3)).unwrap_err();
    txn.abort().unwrap();

    assert_eq!(store.prune_operation_outcomes(50, 10).unwrap(), 1);
    assert_eq!(
        store.operation_outcome("op").unwrap(),
        None,
        "retention evicted the row the replay used to re-read"
    );

    assert_eq!(
        resolve_finalize(Err(flow)).unwrap(),
        FinalizeOutcome::Replayed(OperationOutcomeRecord {
            state: OperationState::Published,
            response: b"ack".to_vec(),
            expiry_unix: Some(10),
            updated_at_unix: 2,
        })
    );
    assert_eq!(
        store.current_serial().unwrap(),
        1,
        "the replay appends no second journal entry"
    );
}
