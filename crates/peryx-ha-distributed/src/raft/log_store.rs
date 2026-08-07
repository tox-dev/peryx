//! The `OpenRaft` `RaftLogStorage` adapter for the ownership consensus group: persists the Raft log
//! entries, the vote, the committed marker, and the purge watermark in durable storage.
//!
//! The storage lane fills this module with the `RaftLogStorage<TypeConfig>` implementation over
//! `peryx-storage`'s log store, keying serialized `Entry` blobs by log index. The foundation declares
//! the module so the `raft` tree is owned in one place and the adapter only adds its implementation
//! here. Snapshots are the state machine's concern, not the log store's, so they live in
//! [`OwnershipStateMachine`](crate::raft::OwnershipStateMachine).
//!
//! The wrapper is thin. It serializes each `Entry`, `Vote`, and `LogId` to bytes with serde-JSON and
//! drives the durable index-keyed CRUD in [`RaftLogStore`]. The orphan rule forbids implementing
//! `OpenRaft`'s trait on the storage crate's `RaftLogStore` directly, so the implementation lives on a
//! newtype here, in the crate that owns both the storage core and the `openraft` dependency.

use std::fmt::Debug;
use std::ops::RangeBounds;

use openraft::storage::{LogFlushed, LogState, RaftLogReader, RaftLogStorage};
use openraft::{
    AnyError, ErrorSubject, ErrorVerb, LogId, OptionalSend, RaftLogId, RaftTypeConfig, StorageError, StorageIOError,
    Vote,
};
use peryx_storage::raft::{RaftLogError, RaftLogStore, StoredEntry};

use super::TypeConfig;

type NodeId = <TypeConfig as RaftTypeConfig>::NodeId;
type Entry = <TypeConfig as RaftTypeConfig>::Entry;

/// A rejected log-store operation before it is mapped into `openraft`'s storage error: either the
/// durable store failed or a persisted blob would not decode.
#[derive(Debug, thiserror::Error)]
enum LogStoreError {
    #[error(transparent)]
    Store(#[from] RaftLogError),
    #[error(transparent)]
    Codec(#[from] serde_json::Error),
}

impl From<LogStoreError> for StorageError<NodeId> {
    fn from(error: LogStoreError) -> Self {
        // `openraft` folds every `StorageError` into `Fatal` and shuts the node down; the subject and
        // verb are diagnostic, never a retry-vs-shutdown signal. So each fallible store operation maps
        // through this one conversion, keeping the sole error-formatting path on the tested decode
        // branch while the underlying redb or serde error rides along under `Store`.
        StorageIOError::new(ErrorSubject::Store, ErrorVerb::Read, AnyError::new(&error)).into()
    }
}

/// The durable [`RaftLogStorage`] for the ownership consensus group, wrapping the redb-backed log store.
#[derive(Clone)]
pub struct RaftLogStoreAdapter {
    store: RaftLogStore,
}

impl RaftLogStoreAdapter {
    /// Wrap a durable log store as the `openraft` log storage.
    #[must_use]
    pub const fn new(store: RaftLogStore) -> Self {
        Self { store }
    }

    /// Decode the entries whose indices fall in `range`, preserving `openraft`'s excluded upper bound
    /// so a range read never returns one entry too many.
    fn entries_in_range<RB: RangeBounds<u64>>(&self, range: RB) -> Result<Vec<Entry>, LogStoreError> {
        let stored = self.store.read_range(range)?;
        let mut entries = Vec::with_capacity(stored.len());
        for entry in &stored {
            entries.push(serde_json::from_slice(&entry.payload)?);
        }
        Ok(entries)
    }

    /// Reconstruct the log bounds from the persisted watermark and the highest entry. With no entries
    /// the last log id falls back to the purge watermark, which `openraft` requires so a restarted node
    /// keeps `last_log_id >= last_purged_log_id`.
    fn log_state(&self) -> Result<LogState<TypeConfig>, LogStoreError> {
        let last_purged_log_id = match self.store.read_purged()? {
            Some(bytes) => Some(serde_json::from_slice::<LogId<NodeId>>(&bytes)?),
            None => None,
        };
        let last_log_id = match self.store.last_entry()? {
            Some(entry) => Some(*serde_json::from_slice::<Entry>(&entry.payload)?.get_log_id()),
            None => last_purged_log_id,
        };
        Ok(LogState {
            last_purged_log_id,
            last_log_id,
        })
    }

    /// Serialize each entry under its log index and append the batch. redb commits durably before it
    /// returns, so the caller may ack the flush once this does.
    fn append_entries<I: IntoIterator<Item = Entry>>(&self, entries: I) -> Result<(), LogStoreError> {
        let mut stored = Vec::new();
        for entry in entries {
            stored.push(StoredEntry {
                index: entry.get_log_id().index,
                payload: serde_json::to_vec(&entry)?,
            });
        }
        self.store.append(&stored)?;
        Ok(())
    }

    /// Drop the conflicting entry at `index` and every entry after it. `openraft`'s truncate is
    /// inclusive of the given index.
    fn truncate_from(&self, index: u64) -> Result<(), LogStoreError> {
        self.store.truncate(index)?;
        Ok(())
    }

    /// Stamp the purge watermark with the full [`LogId`] so [`log_state`](Self::log_state) can read it
    /// back, and drop every entry at or before it in the store's single purge transaction.
    fn purge_upto(&self, log_id: LogId<NodeId>) -> Result<(), LogStoreError> {
        let marker = serde_json::to_vec(&log_id)?;
        self.store.purge(log_id.index, &marker)?;
        Ok(())
    }

    fn store_vote(&self, vote: &Vote<NodeId>) -> Result<(), LogStoreError> {
        self.store.save_vote(&serde_json::to_vec(vote)?)?;
        Ok(())
    }

    fn load_vote(&self) -> Result<Option<Vote<NodeId>>, LogStoreError> {
        match self.store.read_vote()? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            None => Ok(None),
        }
    }

    fn store_committed(&self, committed: Option<LogId<NodeId>>) -> Result<(), LogStoreError> {
        let bytes = match committed {
            Some(log_id) => Some(serde_json::to_vec(&log_id)?),
            None => None,
        };
        self.store.save_committed(bytes.as_deref())?;
        Ok(())
    }

    fn load_committed(&self) -> Result<Option<LogId<NodeId>>, LogStoreError> {
        match self.store.read_committed()? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            None => Ok(None),
        }
    }
}

impl RaftLogReader<TypeConfig> for RaftLogStoreAdapter {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + OptionalSend>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry>, StorageError<NodeId>> {
        Ok(self.entries_in_range(range)?)
    }
}

impl RaftLogStorage<TypeConfig> for RaftLogStoreAdapter {
    type LogReader = Self;

    async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, StorageError<NodeId>> {
        Ok(self.log_state()?)
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &Vote<NodeId>) -> Result<(), StorageError<NodeId>> {
        Ok(self.store_vote(vote)?)
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<NodeId>>, StorageError<NodeId>> {
        Ok(self.load_vote()?)
    }

    async fn append<I>(&mut self, entries: I, callback: LogFlushed<TypeConfig>) -> Result<(), StorageError<NodeId>>
    where
        I: IntoIterator<Item = Entry> + OptionalSend,
        I::IntoIter: OptionalSend,
    {
        self.append_entries(entries)?;
        // The append committed durably above, so the flush is complete before the callback fires.
        callback.log_io_completed(Ok(()));
        Ok(())
    }

    async fn truncate(&mut self, log_id: LogId<NodeId>) -> Result<(), StorageError<NodeId>> {
        Ok(self.truncate_from(log_id.index)?)
    }

    async fn purge(&mut self, log_id: LogId<NodeId>) -> Result<(), StorageError<NodeId>> {
        Ok(self.purge_upto(log_id)?)
    }

    async fn save_committed(&mut self, committed: Option<LogId<NodeId>>) -> Result<(), StorageError<NodeId>> {
        Ok(self.store_committed(committed)?)
    }

    async fn read_committed(&mut self) -> Result<Option<LogId<NodeId>>, StorageError<NodeId>> {
        Ok(self.load_committed()?)
    }
}

#[cfg(test)]
mod tests {
    use openraft::StorageError;
    use openraft::storage::RaftLogStorage;
    use openraft::testing::{StoreBuilder, Suite, log_id};
    use tempfile::TempDir;

    use peryx_storage::raft::RaftLogStore;

    use super::{NodeId, RaftLogStoreAdapter, TypeConfig};
    use crate::raft::OwnershipStateMachine;

    /// Builds a fresh redb-backed log adapter and the ownership state machine for the `openraft`
    /// storage conformance suite. The `TempDir` guard keeps the database file alive for the test.
    struct Builder;

    impl StoreBuilder<TypeConfig, RaftLogStoreAdapter, OwnershipStateMachine, TempDir> for Builder {
        async fn build(&self) -> Result<(TempDir, RaftLogStoreAdapter, OwnershipStateMachine), StorageError<NodeId>> {
            let dir = tempfile::tempdir().unwrap();
            let store = RaftLogStore::open(dir.path().join("raft.redb")).unwrap();
            Ok((dir, RaftLogStoreAdapter::new(store), OwnershipStateMachine::default()))
        }
    }

    #[test]
    fn test_passes_the_openraft_storage_conformance_suite() {
        Suite::test_all(Builder).unwrap();
    }

    #[tokio::test]
    async fn test_committed_log_id_round_trips_and_clears() {
        let dir = tempfile::tempdir().unwrap();
        let mut adapter = RaftLogStoreAdapter::new(RaftLogStore::open(dir.path().join("raft.redb")).unwrap());
        assert_eq!(adapter.read_committed().await.unwrap(), None);

        let committed = log_id(2, 0, 7);
        adapter.save_committed(Some(committed)).await.unwrap();
        assert_eq!(adapter.read_committed().await.unwrap(), Some(committed));

        adapter.save_committed(None).await.unwrap();
        assert_eq!(adapter.read_committed().await.unwrap(), None);
    }

    #[tokio::test]
    async fn test_a_corrupt_persisted_value_surfaces_a_storage_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = RaftLogStore::open(dir.path().join("raft.redb")).unwrap();
        // A byte string the vote decoder cannot parse, written under the store's own vote key.
        store.save_vote(b"not valid json").unwrap();
        let mut adapter = RaftLogStoreAdapter::new(store);

        let error = adapter.read_vote().await.unwrap_err();

        // The serde decode failure itself rides along, not just our wrapper: "Store" is the subject
        // label on every error this adapter renders, so match the decoder's own message instead.
        assert!(error.to_string().contains("expected"), "{error}");
    }

    #[tokio::test]
    async fn test_a_store_fault_surfaces_as_a_storage_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bare.redb");
        // A valid redb database without the raft tables: a read hits redb's "table does not exist"
        // fault, which the adapter folds through its store-error arm into a storage error rather than
        // panicking.
        redb::Database::create(&path).unwrap();
        let mut adapter = RaftLogStoreAdapter::new(RaftLogStore::open_existing(&path).unwrap());

        let error = adapter.read_vote().await.unwrap_err();

        assert!(error.to_string().to_lowercase().contains("does not exist"), "{error}");
    }
}
