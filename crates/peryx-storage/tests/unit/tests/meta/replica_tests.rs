use crate::meta::MetaError;

fn write_replica(txn: &mut crate::meta::DriverTxn<'_>) -> Result<((), Vec<Vec<u8>>), MetaError> {
    txn.put("alpha\0upload", b"record")?;
    txn.put_local("replication\0state", b"1")?;
    Ok(((), vec![b"event".to_vec()]))
}

#[test]
fn test_replica_txn_copies_rows_journal_and_serial() {
    let (_dir, store) = super::store();

    store.commit_replica_txn(0, write_replica).unwrap();

    assert_eq!(store.current_serial().unwrap(), 1);
    assert_eq!(
        store.get_driver_value("alpha\0upload").unwrap().as_deref(),
        Some(b"record".as_slice())
    );
    assert_eq!(
        store.journal_after(0, 10).unwrap(),
        vec![crate::meta::JournalRecord {
            serial: 1,
            payload: b"event".to_vec(),
            mutations: vec![crate::meta::DriverMutation::Put {
                key: "alpha\0upload".to_owned(),
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

    let result = store.commit_replica_txn(0, write_replica);

    assert!(matches!(
        result,
        Err(MetaError::ReplicaSerialConflict { expected: 0, actual: 1 })
    ));
    assert!(store.get_driver_value("alpha\0upload").unwrap().is_none());
    assert!(store.journal_after(1, 10).unwrap().is_empty());
}
