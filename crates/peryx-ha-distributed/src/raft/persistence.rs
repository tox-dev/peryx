//! Entries and markers remain opaque so persistence transactions do not depend on `OpenRaft` types.
//!
//! A separate redb database prevents log writes from contending with metadata commits. One
//! transaction covers each append batch, conflict truncation, compaction purge, or snapshot pair.

use std::ops::Bound::{Included, Unbounded};
use std::ops::RangeBounds;
use std::path::Path;
use std::sync::Arc;

use redb::{Database, ReadableDatabase as _, ReadableTable as _, TableDefinition};

/// redb compares integer keys by value, so range scans preserve log-index order without key encoding.
const RAFT_LOG: TableDefinition<u64, &[u8]> = TableDefinition::new("raft_log");
const RAFT_META: TableDefinition<&str, &[u8]> = TableDefinition::new("raft_meta");

const VOTE_KEY: &str = "vote";
const COMMITTED_KEY: &str = "committed";
const PURGED_KEY: &str = "purged";
const SNAPSHOT_META_KEY: &str = "snapshot_meta";
const SNAPSHOT_DATA_KEY: &str = "snapshot_data";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEntry {
    pub index: u64,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSnapshot {
    pub meta: Vec<u8>,
    pub data: Vec<u8>,
}

/// The adapter maps redb failures to fatal `openraft` storage errors and has no operation-specific
/// recovery path.
pub type RaftLogError = redb::Error;

/// Clones share the same redb database.
#[derive(Debug, Clone)]
pub struct RaftLogStore {
    db: Arc<Database>,
}

impl RaftLogStore {
    /// Opens or creates the database and initializes its tables before returning.
    ///
    /// # Errors
    /// Returns a store error on database open or table initialization failure.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, RaftLogError> {
        let db = Database::create(path)?;
        let txn = db.begin_write()?;
        {
            txn.open_table(RAFT_LOG)?;
            txn.open_table(RAFT_META)?;
        }
        txn.commit()?;
        Ok(Self { db: Arc::new(db) })
    }

    /// Opens an existing database without creating files or tables.
    ///
    /// # Errors
    /// Returns a store error if opening the database fails.
    pub fn open_existing(path: impl AsRef<Path>) -> Result<Self, RaftLogError> {
        Ok(Self {
            db: Arc::new(Database::open(path)?),
        })
    }

    /// Writes the batch in one transaction and replaces entries with matching indices, making retries
    /// idempotent after conflict resolution.
    ///
    /// # Errors
    /// Returns a store error if the transaction commit fails.
    pub fn append(&self, entries: &[StoredEntry]) -> Result<(), RaftLogError> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(RAFT_LOG)?;
            for entry in entries {
                table.insert(entry.index, entry.payload.as_slice())?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// Returns entries in `range` by ascending index.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn read_range<R: RangeBounds<u64>>(&self, range: R) -> Result<Vec<StoredEntry>, RaftLogError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(RAFT_LOG)?;
        let mut entries = Vec::new();
        for entry in table.range::<u64>((range.start_bound().cloned(), range.end_bound().cloned()))? {
            let (index, payload) = entry?;
            entries.push(StoredEntry {
                index: index.value(),
                payload: payload.value().to_vec(),
            });
        }
        Ok(entries)
    }

    /// Removes the divergent suffix starting at `from`.
    ///
    /// # Errors
    /// Returns a store error if the transaction commit fails.
    pub fn truncate(&self, from: u64) -> Result<(), RaftLogError> {
        self.remove_indices((Included(from), Unbounded), None)
    }

    /// Removes entries through `upto` and records `purged_marker` in the same transaction.
    ///
    /// # Errors
    /// Returns a store error if the transaction commit fails.
    pub fn purge(&self, upto: u64, purged_marker: &[u8]) -> Result<(), RaftLogError> {
        self.remove_indices((Unbounded, Included(upto)), Some(purged_marker))
    }

    /// Collects keys before deletion because the range scan holds an immutable table borrow.
    fn remove_indices(
        &self,
        bounds: (std::ops::Bound<u64>, std::ops::Bound<u64>),
        purged_marker: Option<&[u8]>,
    ) -> Result<(), RaftLogError> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(RAFT_LOG)?;
            let mut doomed = Vec::new();
            for entry in table.range::<u64>(bounds)? {
                doomed.push(entry?.0.value());
            }
            for index in doomed {
                table.remove(index)?;
            }
        }
        if let Some(marker) = purged_marker {
            txn.open_table(RAFT_META)?.insert(PURGED_KEY, marker)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Returns the highest-indexed entry, or `None` when the log is empty.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn last_entry(&self) -> Result<Option<StoredEntry>, RaftLogError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(RAFT_LOG)?;
        Ok(table.last()?.map(|(index, payload)| StoredEntry {
            index: index.value(),
            payload: payload.value().to_vec(),
        }))
    }

    /// Replaces the persisted vote.
    ///
    /// # Errors
    /// Returns a store error if the transaction commit fails.
    pub fn save_vote(&self, vote: &[u8]) -> Result<(), RaftLogError> {
        self.put_meta(VOTE_KEY, Some(vote))
    }

    /// Returns the persisted vote, or `None` before the first save.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn read_vote(&self) -> Result<Option<Vec<u8>>, RaftLogError> {
        self.get_meta(VOTE_KEY)
    }

    /// Replaces the committed marker, or clears it when `committed` is `None`.
    ///
    /// # Errors
    /// Returns a store error if the transaction commit fails.
    pub fn save_committed(&self, committed: Option<&[u8]>) -> Result<(), RaftLogError> {
        self.put_meta(COMMITTED_KEY, committed)
    }

    /// Returns the committed marker, or `None` when no marker exists.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn read_committed(&self) -> Result<Option<Vec<u8>>, RaftLogError> {
        self.get_meta(COMMITTED_KEY)
    }

    /// Returns the last purged entry marker, or `None` before the first purge.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn read_purged(&self) -> Result<Option<Vec<u8>>, RaftLogError> {
        self.get_meta(PURGED_KEY)
    }

    /// Replaces snapshot metadata and data in one transaction to prevent mismatched pairs.
    ///
    /// # Errors
    /// Returns a store error if the transaction commit fails.
    pub fn save_snapshot(&self, meta: &[u8], data: &[u8]) -> Result<(), RaftLogError> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(RAFT_META)?;
            table.insert(SNAPSHOT_META_KEY, meta)?;
            table.insert(SNAPSHOT_DATA_KEY, data)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Returns `None` unless both snapshot records exist.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn read_snapshot(&self) -> Result<Option<StoredSnapshot>, RaftLogError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(RAFT_META)?;
        let (Some(meta), Some(data)) = (table.get(SNAPSHOT_META_KEY)?, table.get(SNAPSHOT_DATA_KEY)?) else {
            return Ok(None);
        };
        Ok(Some(StoredSnapshot {
            meta: meta.value().to_vec(),
            data: data.value().to_vec(),
        }))
    }

    fn put_meta(&self, key: &str, value: Option<&[u8]>) -> Result<(), RaftLogError> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(RAFT_META)?;
            match value {
                Some(bytes) => {
                    table.insert(key, bytes)?;
                }
                None => {
                    table.remove(key)?;
                }
            }
        }
        txn.commit()?;
        Ok(())
    }

    fn get_meta(&self, key: &str) -> Result<Option<Vec<u8>>, RaftLogError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(RAFT_META)?;
        Ok(table.get(key)?.map(|value| value.value().to_vec()))
    }
}
