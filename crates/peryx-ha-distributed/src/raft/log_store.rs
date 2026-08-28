//! `OpenRaft` serialization and redb persistence share this distributed-availability boundary.

use std::fmt::Debug;
use std::ops::RangeBounds;

use openraft::storage::{LogFlushed, LogState, RaftLogReader, RaftLogStorage};
use openraft::{
    AnyError, ErrorSubject, ErrorVerb, LogId, OptionalSend, RaftLogId, RaftTypeConfig, StorageError, StorageIOError,
    Vote,
};

use super::TypeConfig;
use super::persistence::{RaftLogError, RaftLogStore, StoredEntry};

type NodeId = <TypeConfig as RaftTypeConfig>::NodeId;
type Entry = <TypeConfig as RaftTypeConfig>::Entry;

#[derive(Debug, thiserror::Error)]
enum LogStoreError {
    #[error(transparent)]
    Store(#[from] RaftLogError),
    #[error(transparent)]
    Codec(#[from] serde_json::Error),
}

#[derive(Clone)]
pub struct RaftLogStoreAdapter {
    store: RaftLogStore,
}

impl RaftLogStoreAdapter {
    #[must_use]
    pub const fn new(store: RaftLogStore) -> Self {
        Self { store }
    }

    fn entries_in_range<RB: RangeBounds<u64>>(&self, range: RB) -> Result<Vec<Entry>, LogStoreError> {
        let stored = self.store.read_range(range)?;
        let mut entries = Vec::with_capacity(stored.len());
        for entry in &stored {
            entries.push(serde_json::from_slice(&entry.payload)?);
        }
        Ok(entries)
    }

    /// Falls back to the purge watermark when no entries remain, preserving `OpenRaft`'s
    /// `last_log_id >= last_purged_log_id` invariant after restart.
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

    /// `OpenRaft` truncation includes the conflicting entry at `index`.
    fn truncate_from(&self, index: u64) -> Result<(), LogStoreError> {
        self.store.truncate(index)?;
        Ok(())
    }

    /// Persists the full [`LogId`] watermark and purges through its index in one transaction.
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
        self.entries_in_range(range)
            .map_err(|error| storage_error(&error, ErrorSubject::Logs, ErrorVerb::Read))
    }
}

impl RaftLogStorage<TypeConfig> for RaftLogStoreAdapter {
    type LogReader = Self;

    async fn get_log_state(&mut self) -> Result<LogState<TypeConfig>, StorageError<NodeId>> {
        self.log_state()
            .map_err(|error| storage_error(&error, ErrorSubject::Logs, ErrorVerb::Read))
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &Vote<NodeId>) -> Result<(), StorageError<NodeId>> {
        self.store_vote(vote)
            .map_err(|error| storage_error(&error, ErrorSubject::Vote, ErrorVerb::Write))
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<NodeId>>, StorageError<NodeId>> {
        self.load_vote()
            .map_err(|error| storage_error(&error, ErrorSubject::Vote, ErrorVerb::Read))
    }

    async fn append<I>(&mut self, entries: I, callback: LogFlushed<TypeConfig>) -> Result<(), StorageError<NodeId>>
    where
        I: IntoIterator<Item = Entry> + OptionalSend,
        I::IntoIter: OptionalSend,
    {
        self.append_entries(entries)
            .map_err(|error| storage_error(&error, ErrorSubject::Logs, ErrorVerb::Write))?;
        // redb committed the batch before this callback, satisfying OpenRaft's durability contract.
        callback.log_io_completed(Ok(()));
        Ok(())
    }

    async fn truncate(&mut self, log_id: LogId<NodeId>) -> Result<(), StorageError<NodeId>> {
        self.truncate_from(log_id.index)
            .map_err(|error| storage_error(&error, ErrorSubject::Log(log_id), ErrorVerb::Delete))
    }

    async fn purge(&mut self, log_id: LogId<NodeId>) -> Result<(), StorageError<NodeId>> {
        self.purge_upto(log_id)
            .map_err(|error| storage_error(&error, ErrorSubject::Log(log_id), ErrorVerb::Delete))
    }

    async fn save_committed(&mut self, committed: Option<LogId<NodeId>>) -> Result<(), StorageError<NodeId>> {
        self.store_committed(committed)
            .map_err(|error| storage_error(&error, ErrorSubject::Logs, ErrorVerb::Write))
    }

    async fn read_committed(&mut self) -> Result<Option<LogId<NodeId>>, StorageError<NodeId>> {
        self.load_committed()
            .map_err(|error| storage_error(&error, ErrorSubject::Logs, ErrorVerb::Read))
    }
}

fn storage_error(error: &LogStoreError, subject: ErrorSubject<NodeId>, verb: ErrorVerb) -> StorageError<NodeId> {
    // Storage failures remain fatal in OpenRaft; subject and verb only affect diagnostics.
    StorageIOError::new(subject, verb, AnyError::new(error)).into()
}

#[cfg(test)]
#[path = "../../tests/unit/raft/log_store/tests.rs"]
mod tests;
