use crate::meta::MetaError;

#[test]
fn test_replica_txn_copies_rows_journal_and_serial() {
    let (_dir, store) = super::store();

    store
        .commit_replica_txn(0, |txn| {
            txn.put("pypi\0upload", b"record")?;
            txn.put_local("replication\0state", b"1")?;
            Ok::<_, MetaError>(((), vec![b"event".to_vec()]))
        })
        .unwrap();

    assert_eq!(store.current_serial().unwrap(), 1);
    assert_eq!(
        store.get_driver_value("pypi\0upload").unwrap().as_deref(),
        Some(b"record".as_slice())
    );
    assert_eq!(
        store.journal_after(0, 10).unwrap(),
        vec![crate::meta::JournalRecord {
            serial: 1,
            payload: b"event".to_vec(),
            mutations: vec![crate::meta::DriverMutation::Put {
                key: "pypi\0upload".to_owned(),
                value: b"record".to_vec(),
            }],
            blobs: Vec::new(),
        }]
    );
}

#[test]
fn test_replica_txn_rejects_a_stale_cursor_without_writes() {
    let (_dir, store) = super::store();
    store.next_serial().unwrap();

    let result = store.commit_replica_txn(0, |txn| {
        txn.put("pypi\0upload", b"record")?;
        Ok::<_, MetaError>(((), vec![b"event".to_vec()]))
    });

    assert!(matches!(
        result,
        Err(MetaError::ReplicaSerialConflict { expected: 0, actual: 1 })
    ));
    assert!(store.get_driver_value("pypi\0upload").unwrap().is_none());
    assert!(store.journal_after(1, 10).unwrap().is_empty());
}
