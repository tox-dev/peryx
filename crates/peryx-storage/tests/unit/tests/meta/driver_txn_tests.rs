use crate::meta::{DriverBatch, MetaError};

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
                vec![
                    b"{\"action\":\"invalidate\"}".to_vec(),
                    b"{\"action\":\"invalidate\"}".to_vec(),
                ],
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
            txn.reference_blob("bbbb", 2);
            txn.reference_blob("aaaa", 1);
            txn.reference_blob("aaaa", 1);
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
    assert_eq!(
        records[1].blobs,
        vec![
            crate::meta::DriverBlobReference {
                sha256: "aaaa".to_owned(),
                size: 1,
            },
            crate::meta::DriverBlobReference {
                sha256: "bbbb".to_owned(),
                size: 2,
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
fn test_commit_driver_cache_txn_reads_and_writes_without_advancing_the_serial() {
    let (_dir, store) = super::store();
    store.put_driver_value("k", b"old").unwrap();

    let previous = store
        .commit_driver_cache_txn(|txn| {
            let previous = txn.get("k")?.unwrap();
            txn.put_local("k", b"new")?;
            Ok::<_, MetaError>(previous)
        })
        .unwrap();

    assert_eq!(previous, b"old");
    assert_eq!(store.current_serial().unwrap(), 0);
    assert_eq!(store.get_driver_value("k").unwrap().as_deref(), Some(b"new".as_slice()));
}

#[test]
fn test_commit_driver_cache_txn_rolls_back_when_the_body_errors() {
    let (_dir, store) = super::store();

    let result = store.commit_driver_cache_txn(|txn| {
        txn.put_local("k", b"v")?;
        Err::<(), _>(decode_error())
    });

    assert!(result.is_err(), "the body's error propagates");
    assert!(
        store.get_driver_value("k").unwrap().is_none(),
        "the cache row was not committed"
    );
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

#[test]
fn test_driver_value_update_and_delete_report_prior_state() {
    let (_dir, store) = super::store();
    assert_eq!(
        store
            .update_driver_value("key", |current| {
                assert_eq!(current, None);
                Ok((Some(b"value".to_vec()), "inserted"))
            })
            .unwrap(),
        "inserted"
    );
    assert_eq!(
        store
            .update_driver_value("key", |current| {
                assert_eq!(current, Some(b"value".as_slice()));
                Ok((None, "removed"))
            })
            .unwrap(),
        "removed"
    );
    assert!(!store.delete_driver_value("key").unwrap());
    store.put_driver_value("key", b"value").unwrap();
    assert!(store.delete_driver_value("key").unwrap());
}

#[test]
fn test_driver_batch_applies_puts_and_deletes_with_selected_durability() {
    let (_dir, store) = super::store();
    store.put_driver_value("delete", b"old").unwrap();
    let mut batch = DriverBatch::new();
    batch.put("put".to_owned(), b"new".to_vec());
    batch.delete("delete".to_owned());

    store.commit_driver_batch(&batch, false).unwrap();

    assert_eq!(
        (
            store.get_driver_value("put").unwrap(),
            store.get_driver_value("delete").unwrap(),
        ),
        (Some(b"new".to_vec()), None)
    );
}

#[test]
fn test_driver_policy_snapshot_reads_rows_and_current_revision() {
    let (_dir, store) = super::store();
    store.put_driver_value("scope/a", b"one").unwrap();
    store.put_driver_value("scope/b", b"two").unwrap();
    store
        .commit_driver_txn(|txn| {
            txn.touch_policy_inputs("repository");
            Ok::<_, crate::meta::MetaError>(((), Vec::new()))
        })
        .unwrap();
    let mut seen = Vec::new();
    let mut generation = None;

    store
        .visit_driver_policy_snapshot(
            "scope/",
            "repository",
            |current| {
                generation = Some(current);
                Ok::<_, std::convert::Infallible>(())
            },
            |key, value| {
                seen.push((key.to_owned(), value.to_vec()));
                Ok::<_, std::convert::Infallible>(())
            },
        )
        .unwrap();

    assert_eq!(
        seen,
        vec![
            ("scope/a".to_owned(), b"one".to_vec()),
            ("scope/b".to_owned(), b"two".to_vec())
        ]
    );
    assert_eq!(generation.unwrap().repository, 1);
}

#[test]
fn test_driver_cache_transaction_removes_local_rows() {
    let (_dir, store) = super::store();
    store.put_driver_value("key", b"value").unwrap();
    assert!(store.commit_driver_cache_txn(|txn| txn.remove_local("key")).unwrap());
    assert_eq!(store.get_driver_value("key").unwrap(), None);
}

#[test]
fn test_driver_prefix_operations_stop_at_limits_and_prefix_boundaries() {
    let (_dir, store) = super::store();
    for (key, value) in [
        ("scope/a", b"remove".as_slice()),
        ("scope/b", b"keep".as_slice()),
        ("scopez", b"outside".as_slice()),
    ] {
        store.put_driver_value(key, value).unwrap();
    }
    assert_eq!(
        store
            .remove_driver_values_if("scope/", 1, |value| Ok(value == b"remove"))
            .unwrap(),
        vec!["scope/a"]
    );
    store.put_driver_value("scope/c", b"remove").unwrap();
    assert_eq!(
        store
            .remove_driver_values_if("scope/", 2, |value| Ok(value == b"remove"))
            .unwrap(),
        vec!["scope/c"]
    );
    assert_eq!(store.driver_prefix_keys("scope/").unwrap(), vec!["scope/b"]);
}
