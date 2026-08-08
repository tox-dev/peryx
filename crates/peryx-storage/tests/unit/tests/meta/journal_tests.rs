use crate::meta::{JournalRecord, MetaError, MetaStore};

use super::store;

fn journaled(values: &[&[u8]]) -> (tempfile::TempDir, MetaStore) {
    let (dir, store) = store();
    store
        .commit_driver_txn(|_| Ok::<_, MetaError>(((), values.iter().map(|value| value.to_vec()).collect())))
        .unwrap();
    (dir, store)
}

fn record(serial: u64, payload: &[u8]) -> JournalRecord {
    JournalRecord {
        serial,
        payload: payload.to_vec(),
        mutations: Vec::new(),
        blobs: Vec::new(),
    }
}

fn store_with_incompatible_journal_table(table_name: &'static str) -> (tempfile::TempDir, MetaStore) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let database = redb::Database::create(&path).unwrap();
    let txn = database.begin_write().unwrap();
    {
        let mut serial = txn
            .open_table(redb::TableDefinition::<&str, u64>::new("serial"))
            .unwrap();
        serial.insert("serial", 1).unwrap();
        let mut journal = txn
            .open_table(redb::TableDefinition::<u64, &[u8]>::new("journal"))
            .unwrap();
        journal.insert(1, b"value".as_slice()).unwrap();
        txn.open_table(redb::TableDefinition::<&str, u64>::new(table_name))
            .unwrap();
    }
    txn.commit().unwrap();
    drop(database);
    (dir, MetaStore::open_existing(path).unwrap())
}

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

#[test]
fn test_journal_snapshot_reads_the_head_and_ordered_values() {
    let (_dir, store) = journaled(&[b"first".as_slice(), b"second".as_slice(), b"third".as_slice()]);

    assert_eq!(
        store.journal_snapshot(0, 10).unwrap(),
        crate::meta::JournalSnapshot {
            current_serial: 3,
            records: vec![record(1, b"first"), record(2, b"second"), record(3, b"third")],
        }
    );
}

#[test]
fn test_journal_snapshot_uses_an_exclusive_cursor_and_limit() {
    let (_dir, store) = journaled(&[b"first".as_slice(), b"second".as_slice(), b"third".as_slice()]);

    assert_eq!(
        store.journal_snapshot(1, 1).unwrap(),
        crate::meta::JournalSnapshot {
            current_serial: 3,
            records: vec![record(2, b"second")],
        }
    );
}

#[test]
fn test_journal_snapshot_reads_an_empty_store() {
    let (_dir, store) = store();

    let snapshot = store.journal_snapshot(0, 10).unwrap();
    assert_eq!(snapshot.current_serial, 0);
    assert!(snapshot.records.is_empty());
}

#[test]
fn test_journal_snapshot_reads_an_older_store_without_a_journal_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let database = redb::Database::create(&path).unwrap();
    let txn = database.begin_write().unwrap();
    txn.open_table(redb::TableDefinition::<&str, u64>::new("serial"))
        .unwrap();
    txn.commit().unwrap();
    drop(database);

    let snapshot = MetaStore::open_existing(path).unwrap().journal_snapshot(0, 10).unwrap();
    assert_eq!(snapshot.current_serial, 0);
    assert!(snapshot.records.is_empty());
}

#[test]
fn test_journal_snapshot_rejects_an_incompatible_journal_table() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let database = redb::Database::create(&path).unwrap();
    let txn = database.begin_write().unwrap();
    txn.open_table(redb::TableDefinition::<&str, u64>::new("serial"))
        .unwrap();
    txn.open_table(redb::TableDefinition::<&str, &[u8]>::new("journal"))
        .unwrap();
    txn.commit().unwrap();
    drop(database);

    assert!(matches!(
        MetaStore::open_existing(path).unwrap().journal_snapshot(0, 10),
        Err(MetaError::Table(_))
    ));
}

#[test]
fn test_journal_snapshot_rejects_an_incompatible_mutations_table() {
    let (_dir, store) = store_with_incompatible_journal_table("journal_mutations");

    assert!(matches!(store.journal_snapshot(0, 10), Err(MetaError::Table(_))));
}

#[test]
fn test_journal_snapshot_rejects_an_incompatible_blobs_table() {
    let (_dir, store) = store_with_incompatible_journal_table("journal_blobs");

    assert!(matches!(store.journal_snapshot(0, 10), Err(MetaError::Table(_))));
}

#[test]
fn test_journal_snapshot_reads_an_older_journal_without_replication_tables() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("peryx.redb");
    let database = redb::Database::create(&path).unwrap();
    let txn = database.begin_write().unwrap();
    {
        let mut serial = txn
            .open_table(redb::TableDefinition::<&str, u64>::new("serial"))
            .unwrap();
        serial.insert("serial", 1).unwrap();
        let mut journal = txn
            .open_table(redb::TableDefinition::<u64, &[u8]>::new("journal"))
            .unwrap();
        journal.insert(1, b"value".as_slice()).unwrap();
    }
    txn.commit().unwrap();
    drop(database);

    assert_eq!(
        MetaStore::open_existing(path).unwrap().journal_snapshot(0, 10).unwrap(),
        crate::meta::JournalSnapshot {
            current_serial: 1,
            records: vec![record(1, b"value")],
        }
    );
}

#[test]
fn test_journal_snapshot_past_the_head_returns_an_empty_page() {
    let (_dir, store) = journaled(&[b"value"]);

    let snapshot = store.journal_snapshot(u64::MAX, 10).unwrap();
    assert_eq!(snapshot.current_serial, 1);
    assert!(snapshot.records.is_empty());
}

#[test]
fn test_journal_snapshot_honors_a_zero_limit() {
    let (_dir, store) = journaled(&[b"value"]);

    let snapshot = store.journal_snapshot(0, 0).unwrap();
    assert_eq!(snapshot.current_serial, 1);
    assert!(snapshot.records.is_empty());
}
