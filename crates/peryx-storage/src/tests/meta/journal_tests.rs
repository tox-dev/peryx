use super::store;

#[test]
fn test_serial_starts_at_zero_and_increments() {
    let (_dir, store) = store();
    assert_eq!(store.current_serial().unwrap(), 0);
    assert_eq!(store.next_serial().unwrap(), 1);
    assert_eq!(store.next_serial().unwrap(), 2);
    assert_eq!(store.current_serial().unwrap(), 2);
}

#[test]
fn test_journal_after_pages_from_an_exclusive_serial() {
    let (_dir, store) = super::store();
    store
        .commit_driver_txn(|_| {
            Ok::<_, crate::meta::MetaError>(((), vec![b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]))
        })
        .unwrap();

    let page = store.journal_after(1, 1).unwrap();

    assert_eq!(page.len(), 1);
    assert_eq!(page[0].serial, 2);
    assert_eq!(page[0].payload, b"two");
}

#[test]
fn test_journal_page_reads_serial_and_records_together() {
    let (_dir, store) = super::store();
    store
        .commit_driver_txn(|_| Ok::<_, crate::meta::MetaError>(((), vec![b"one".to_vec(), b"two".to_vec()])))
        .unwrap();

    let (current, records) = store.journal_page_after(0, 1).unwrap();

    assert_eq!(current, 2);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].serial, 1);
    assert_eq!(records[0].payload, b"one");
}
