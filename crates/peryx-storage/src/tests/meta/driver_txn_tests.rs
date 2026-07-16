use crate::meta::MetaError;

fn decode_error() -> MetaError {
    MetaError::from(serde_json::from_str::<serde_json::Value>("{").unwrap_err())
}

#[test]
fn test_commit_driver_txn_journaled_writes_rows_and_advances_the_serial() {
    let (_dir, store) = super::store();

    let value = store
        .commit_driver_txn(|txn| {
            txn.put("k", b"v")?;
            Ok::<_, MetaError>((7_u8, vec![b"{\"action\":\"add\"}".to_vec()]))
        })
        .unwrap();

    assert_eq!(value, 7, "the body's value is returned");
    assert_eq!(
        store.current_serial().unwrap(),
        1,
        "a journal entry allocates the next serial"
    );
    assert_eq!(store.get_driver_value("k").unwrap().as_deref(), Some(b"v".as_slice()));
}

#[test]
fn test_commit_driver_txn_allocates_a_serial_for_each_journal_entry() {
    let (_dir, store) = super::store();

    store
        .commit_driver_txn(|txn| {
            txn.put("k", b"v")?;
            Ok::<_, MetaError>((
                (),
                vec![b"{\"action\":\"yank\"}".to_vec(), b"{\"action\":\"yank\"}".to_vec()],
            ))
        })
        .unwrap();

    assert_eq!(
        store.current_serial().unwrap(),
        2,
        "a batch records one serial per journal entry, in order"
    );
}

#[test]
fn test_commit_driver_txn_records_final_row_changes_on_the_last_serial() {
    let (_dir, store) = super::store();
    store.put_driver_value("delete", b"old").unwrap();

    store
        .commit_driver_txn(|txn| {
            txn.put("put", b"first")?;
            txn.put("put", b"final")?;
            txn.remove("delete")?;
            txn.put_local("local", b"private")?;
            Ok::<_, MetaError>(((), vec![b"one".to_vec(), b"two".to_vec()]))
        })
        .unwrap();

    let records = store.journal_after(0, 10).unwrap();
    assert!(records[0].mutations.is_empty());
    assert_eq!(
        records[1].mutations,
        vec![
            crate::meta::DriverMutation::Delete {
                key: "delete".to_owned(),
            },
            crate::meta::DriverMutation::Put {
                key: "put".to_owned(),
                value: b"final".to_vec(),
            },
        ]
    );
}

#[test]
fn test_commit_driver_txn_without_a_journal_leaves_the_serial_untouched() {
    let (_dir, store) = super::store();
    store.put_driver_value("k", b"old").unwrap();

    store
        .commit_driver_txn(|txn| {
            txn.put("k", b"new")?;
            Ok::<_, MetaError>(((), Vec::new()))
        })
        .unwrap();

    assert_eq!(
        store.current_serial().unwrap(),
        0,
        "an unjournaled commit records no serial"
    );
    assert_eq!(store.get_driver_value("k").unwrap().as_deref(), Some(b"new".as_slice()));
}

#[test]
fn test_commit_driver_txn_rolls_back_when_the_body_errors() {
    let (_dir, store) = super::store();

    let result = store.commit_driver_txn(|txn| {
        txn.put("k", b"v")?;
        Err::<((), Vec<Vec<u8>>), _>(decode_error())
    });

    assert!(result.is_err(), "the body's error propagates");
    assert!(
        store.get_driver_value("k").unwrap().is_none(),
        "the aborted transaction wrote nothing"
    );
}

#[test]
fn test_driver_txn_get_sees_committed_and_absent_keys() {
    let (_dir, store) = super::store();
    store.put_driver_value("present", b"x").unwrap();

    store
        .commit_driver_txn(|txn| {
            assert_eq!(txn.get("present").unwrap().as_deref(), Some(b"x".as_slice()));
            assert!(txn.get("absent").unwrap().is_none());
            Ok::<_, MetaError>(((), Vec::new()))
        })
        .unwrap();
}

#[test]
fn test_driver_txn_upsert_reports_insert_and_replace() {
    let (_dir, store) = super::store();

    let result = store
        .commit_driver_txn(|txn| {
            Ok::<_, MetaError>(((txn.upsert("k", b"first")?, txn.upsert("k", b"second")?), Vec::new()))
        })
        .unwrap();

    assert_eq!(
        (result, store.get_driver_value("k").unwrap()),
        ((true, false), Some(b"second".to_vec()))
    );
}

#[test]
fn test_driver_txn_prefix_stops_at_the_first_key_outside_the_prefix() {
    let (_dir, store) = super::store();
    store.put_driver_value("app/a", b"1").unwrap();
    store.put_driver_value("app/b", b"2").unwrap();
    store.put_driver_value("appz", b"3").unwrap();

    let removed = store
        .commit_driver_txn(|txn| {
            let entries = txn.prefix("app/")?;
            assert_eq!(
                entries,
                vec![("app/a".to_owned(), b"1".to_vec()), ("app/b".to_owned(), b"2".to_vec())],
                "the scan excludes the lexicographically later key that lacks the prefix"
            );
            Ok::<_, MetaError>((txn.remove("app/a")?, Vec::new()))
        })
        .unwrap();

    assert!(removed, "remove reports the key was present");
    assert!(store.get_driver_value("app/a").unwrap().is_none());
    assert_eq!(
        store.get_driver_value("appz").unwrap().as_deref(),
        Some(b"3".as_slice())
    );
}
