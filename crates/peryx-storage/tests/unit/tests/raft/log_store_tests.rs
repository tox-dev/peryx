use tempfile::TempDir;

use crate::raft::{RaftLogStore, StoredEntry, StoredSnapshot};

fn snapshot(meta: &[u8], data: &[u8]) -> StoredSnapshot {
    StoredSnapshot {
        meta: meta.to_vec(),
        data: data.to_vec(),
    }
}

fn store() -> (TempDir, RaftLogStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = RaftLogStore::open(dir.path().join("raft.redb")).unwrap();
    (dir, store)
}

#[test]
fn test_opening_a_non_database_file_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("not-a-db.redb");
    std::fs::write(&path, b"these bytes are not a redb database").unwrap();

    // A garbage file drives redb's open failure through the store's error type.
    let Err(error) = RaftLogStore::open_existing(&path) else {
        panic!("opening a non-database file must fail");
    };
    assert!(error.to_string().to_lowercase().contains("data"), "{error}");
}

#[test]
fn test_reading_a_store_whose_tables_are_absent_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bare.redb");
    // A valid redb database that was never given the raft tables: opening succeeds, but a read of a
    // table that does not exist surfaces the store's error.
    redb::Database::create(&path).unwrap();
    let store = RaftLogStore::open_existing(&path).unwrap();

    let error = store.read_range(..).unwrap_err();
    assert!(error.to_string().to_lowercase().contains("does not exist"), "{error}");
}

fn entry(index: u64, payload: &[u8]) -> StoredEntry {
    StoredEntry {
        index,
        payload: payload.to_vec(),
    }
}

fn seed(store: &RaftLogStore, indices: impl IntoIterator<Item = u64>) {
    let entries: Vec<StoredEntry> = indices
        .into_iter()
        .map(|index| entry(index, &[u8::try_from(index).unwrap()]))
        .collect();
    store.append(&entries).unwrap();
}

fn indices(entries: &[StoredEntry]) -> Vec<u64> {
    entries.iter().map(|entry| entry.index).collect()
}

#[test]
fn test_append_then_read_returns_entries_in_index_order() {
    let (_dir, store) = store();
    // Appended out of order, they still read back ascending by index.
    store
        .append(&[entry(2, b"two"), entry(1, b"one"), entry(3, b"three")])
        .unwrap();

    let entries = store.read_range(..).unwrap();

    assert_eq!(entries, vec![entry(1, b"one"), entry(2, b"two"), entry(3, b"three")]);
}

#[test]
fn test_read_range_honors_its_bounds() {
    let (_dir, store) = store();
    seed(&store, 1..=5);

    assert_eq!(indices(&store.read_range(..).unwrap()), vec![1, 2, 3, 4, 5]);
    assert_eq!(indices(&store.read_range(3..).unwrap()), vec![3, 4, 5]);
    assert_eq!(indices(&store.read_range(..3).unwrap()), vec![1, 2]);
    assert_eq!(indices(&store.read_range(2..=4).unwrap()), vec![2, 3, 4]);
    assert_eq!(indices(&store.read_range(2..4).unwrap()), vec![2, 3]);
}

#[test]
fn test_read_range_on_an_empty_log_is_empty() {
    let (_dir, store) = store();
    assert_eq!(store.read_range(..).unwrap(), Vec::new());
}

#[test]
fn test_append_overwrites_an_existing_index() {
    let (_dir, store) = store();
    store.append(&[entry(1, b"first")]).unwrap();
    store.append(&[entry(1, b"second")]).unwrap();

    assert_eq!(store.read_range(..).unwrap(), vec![entry(1, b"second")]);
}

#[test]
fn test_truncate_removes_the_suffix_from_the_index() {
    let (_dir, store) = store();
    seed(&store, 1..=5);

    store.truncate(3).unwrap();

    assert_eq!(indices(&store.read_range(..).unwrap()), vec![1, 2]);
    assert_eq!(store.last_entry().unwrap(), Some(entry(2, &[2])));
}

#[test]
fn test_truncate_past_the_end_leaves_the_log_intact() {
    let (_dir, store) = store();
    seed(&store, 1..=3);

    store.truncate(10).unwrap();

    assert_eq!(indices(&store.read_range(..).unwrap()), vec![1, 2, 3]);
}

#[test]
fn test_purge_removes_the_prefix_and_records_the_watermark() {
    let (_dir, store) = store();
    seed(&store, 1..=5);

    store.purge(3, b"purged-at-3").unwrap();

    assert_eq!(indices(&store.read_range(..).unwrap()), vec![4, 5]);
    assert_eq!(store.read_purged().unwrap(), Some(b"purged-at-3".to_vec()));
}

#[test]
fn test_read_purged_is_none_before_any_purge() {
    let (_dir, store) = store();
    seed(&store, 1..=3);
    assert_eq!(store.read_purged().unwrap(), None);
}

#[test]
fn test_last_entry_reports_the_highest_index() {
    let (_dir, store) = store();
    seed(&store, 1..=3);
    assert_eq!(store.last_entry().unwrap(), Some(entry(3, &[3])));
}

#[test]
fn test_last_entry_on_an_empty_log_is_none() {
    let (_dir, store) = store();
    assert_eq!(store.last_entry().unwrap(), None);
}

#[test]
fn test_vote_round_trips_and_is_none_before_a_save() {
    let (_dir, store) = store();
    assert_eq!(store.read_vote().unwrap(), None);

    store.save_vote(b"vote-1").unwrap();
    assert_eq!(store.read_vote().unwrap(), Some(b"vote-1".to_vec()));

    // A later vote overwrites the prior one.
    store.save_vote(b"vote-2").unwrap();
    assert_eq!(store.read_vote().unwrap(), Some(b"vote-2".to_vec()));
}

#[test]
fn test_committed_round_trips_and_clears() {
    let (_dir, store) = store();
    assert_eq!(store.read_committed().unwrap(), None);

    store.save_committed(Some(b"committed-7")).unwrap();
    assert_eq!(store.read_committed().unwrap(), Some(b"committed-7".to_vec()));

    store.save_committed(None).unwrap();
    assert_eq!(store.read_committed().unwrap(), None);
}

#[test]
fn test_snapshot_round_trips_and_is_none_before_a_save() {
    let (_dir, store) = store();
    assert_eq!(store.read_snapshot().unwrap(), None);

    store.save_snapshot(b"meta-1", b"data-1").unwrap();
    assert_eq!(store.read_snapshot().unwrap(), Some(snapshot(b"meta-1", b"data-1")));

    // A later snapshot replaces the prior metadata and data.
    store.save_snapshot(b"meta-2", b"data-2").unwrap();
    assert_eq!(store.read_snapshot().unwrap(), Some(snapshot(b"meta-2", b"data-2")));
}

#[test]
fn test_state_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("raft.redb");
    {
        let store = RaftLogStore::open(&path).unwrap();
        seed(&store, 1..=4);
        store.save_vote(b"vote").unwrap();
        store.save_committed(Some(b"committed")).unwrap();
        store.purge(1, b"purged-at-1").unwrap();
        store.save_snapshot(b"snap-meta", b"snap-data").unwrap();
    }

    let store = RaftLogStore::open_existing(&path).unwrap();
    assert_eq!(indices(&store.read_range(..).unwrap()), vec![2, 3, 4]);
    assert_eq!(store.read_vote().unwrap(), Some(b"vote".to_vec()));
    assert_eq!(store.read_committed().unwrap(), Some(b"committed".to_vec()));
    assert_eq!(store.read_purged().unwrap(), Some(b"purged-at-1".to_vec()));
    assert_eq!(
        store.read_snapshot().unwrap(),
        Some(snapshot(b"snap-meta", b"snap-data"))
    );
}
