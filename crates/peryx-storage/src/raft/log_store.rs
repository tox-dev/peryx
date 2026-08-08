//! The durable, type-independent core of the Raft log store: a redb-backed record of the log
//! entries, the persisted vote, the committed marker, and the purge watermark.
//!
//! `openraft`'s `RaftLogStorage` trait is generic over a `RaftTypeConfig`, and its associated types
//! (the entry, the vote, the log id) churn across releases. The persistence underneath does not: it
//! is an index-keyed record of opaque entry bytes plus a handful of singletons. This module owns that
//! persistence. It stores each entry as a serialized blob keyed by its log index and keeps the vote,
//! the committed marker, and the purge watermark as opaque blobs the adapter serializes, so the CRUD
//! is stable whatever the concrete `RaftTypeConfig` turns out to be.
//!
//! The thin `impl RaftLogStorage<C>` adapter that serializes `C::Entry`, `Vote`, and `LogId` into
//! these bytes lives above, in the crate that pins the `openraft` version and the type config. What
//! lives here is only the durable store and its append, range-read, suffix-truncate, prefix-purge,
//! and singleton round-trips, all under redb's single-writer serialization so a conflict truncate or
//! a compaction purge lands atomically.
//!
//! The store keeps its own redb database, separate from the metadata store, so the log's high write
//! volume never contends with metadata commits and the two evolve independently.

use std::ops::Bound::{Included, Unbounded};
use std::ops::RangeBounds;
use std::path::Path;
use std::sync::Arc;

use redb::{Database, ReadableDatabase as _, ReadableTable as _, TableDefinition};

/// The log entries, keyed by log index so a range scan yields them in ascending index order. redb
/// compares its integer keys numerically, so the scan needs no manual key encoding.
const RAFT_LOG: TableDefinition<u64, &[u8]> = TableDefinition::new("raft_log");
/// The log's singletons: the persisted vote, the committed marker, and the purge watermark, each an
/// opaque blob the adapter above serializes.
const RAFT_META: TableDefinition<&str, &[u8]> = TableDefinition::new("raft_meta");

const VOTE_KEY: &str = "vote";
const COMMITTED_KEY: &str = "committed";
const PURGED_KEY: &str = "purged";
const SNAPSHOT_META_KEY: &str = "snapshot_meta";
const SNAPSHOT_DATA_KEY: &str = "snapshot_data";

/// One stored log entry: its index and the opaque bytes the adapter serialized for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredEntry {
    pub index: u64,
    pub payload: Vec<u8>,
}

/// A stored snapshot: the opaque metadata and data bytes the adapter serialized, held together so a
/// reader always gets a matching pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSnapshot {
    pub meta: Vec<u8>,
    pub data: Vec<u8>,
}

/// A rejected Raft log-store operation, always leaving the store unchanged.
///
/// redb reports each failing operation as one of several sub-errors - opening the database, beginning a
/// transaction, opening a table, a storage fault, a commit - that all fold into its own [`redb::Error`],
/// so this is that type. Distinguishing the sub-errors bought nothing here: the adapter above renders
/// any store failure as a single fatal `openraft` storage error, and no deterministic test can tell one
/// redb fault from another. Keeping one error keeps the one conversion on a tested path.
pub type RaftLogError = redb::Error;

/// A durable Raft log over redb. Cloning shares the one underlying database, so the `openraft` adapter
/// and its log reader hand out cheap handles to the same store.
#[derive(Debug, Clone)]
pub struct RaftLogStore {
    db: Arc<Database>,
}

impl RaftLogStore {
    /// Open the log store at `path`, creating the database and its tables if they are absent so a
    /// later read never races a missing table.
    ///
    /// # Errors
    /// Returns a store error if the database cannot be opened or initialized.
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

    /// Open an existing log store without creating files or tables.
    ///
    /// # Errors
    /// Returns a store error if the database cannot be opened.
    pub fn open_existing(path: impl AsRef<Path>) -> Result<Self, RaftLogError> {
        Ok(Self {
            db: Arc::new(Database::open(path)?),
        })
    }

    /// Append `entries` to the log, each under its own index.
    ///
    /// A conflict resolution truncates the divergent suffix first and then appends, so this writes
    /// each index unconditionally; an index already present is overwritten, which keeps a retried
    /// append idempotent. The whole batch lands in one transaction.
    ///
    /// # Errors
    /// Returns a store error if the write cannot be committed.
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

    /// Read the entries whose indices fall in `range`, in ascending index order.
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

    /// Remove every entry at or after `from`, the divergent suffix a conflict discards.
    ///
    /// # Errors
    /// Returns a store error if the write cannot be committed.
    pub fn truncate(&self, from: u64) -> Result<(), RaftLogError> {
        self.remove_indices((Included(from), Unbounded), None)
    }

    /// Remove every entry at or before `upto` and record `purged_marker` as the purge watermark, the
    /// compaction that reclaims applied entries. The delete and the watermark land together, so a
    /// crash never leaves entries purged without the watermark that records how far.
    ///
    /// # Errors
    /// Returns a store error if the write cannot be committed.
    pub fn purge(&self, upto: u64, purged_marker: &[u8]) -> Result<(), RaftLogError> {
        self.remove_indices((Unbounded, Included(upto)), Some(purged_marker))
    }

    /// Remove the entries in `bounds` and, when purging, stamp the watermark, all in one transaction.
    /// A range scan borrows the table immutably, so the keys are collected before the removals begin.
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

    /// The highest-indexed entry still present, or `None` when the log holds no entries.
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

    /// Persist the vote, overwriting any prior one.
    ///
    /// # Errors
    /// Returns a store error if the write cannot be committed.
    pub fn save_vote(&self, vote: &[u8]) -> Result<(), RaftLogError> {
        self.put_meta(VOTE_KEY, Some(vote))
    }

    /// Read the persisted vote, or `None` before the first vote is saved.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn read_vote(&self) -> Result<Option<Vec<u8>>, RaftLogError> {
        self.get_meta(VOTE_KEY)
    }

    /// Persist the committed marker, or clear it when `committed` is `None`.
    ///
    /// # Errors
    /// Returns a store error if the write cannot be committed.
    pub fn save_committed(&self, committed: Option<&[u8]>) -> Result<(), RaftLogError> {
        self.put_meta(COMMITTED_KEY, committed)
    }

    /// Read the committed marker, or `None` when none is stored.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn read_committed(&self) -> Result<Option<Vec<u8>>, RaftLogError> {
        self.get_meta(COMMITTED_KEY)
    }

    /// Read the purge watermark, the marker for the last purged entry, or `None` before any purge.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn read_purged(&self) -> Result<Option<Vec<u8>>, RaftLogError> {
        self.get_meta(PURGED_KEY)
    }

    /// Persist the current snapshot: its metadata and its serialized data, replacing any prior
    /// snapshot. Both land in one transaction, so a reader never pairs new metadata with stale data.
    ///
    /// # Errors
    /// Returns a store error if the write cannot be committed.
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

    /// Read the current snapshot, or `None` before any snapshot is saved. The metadata and data are
    /// written together, so a half-written pair reads as no snapshot rather than a mismatched one.
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
