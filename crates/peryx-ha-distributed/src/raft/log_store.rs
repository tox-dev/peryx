//! `OpenRaft` serialization and redb persistence share this distributed-availability boundary.

use std::fmt::Debug;
use std::io;
use std::ops::RangeBounds;

use openraft::OptionalSend;
use openraft::storage::{IOFlushed, LogState, RaftLogReader, RaftLogStorage};

use super::persistence::{RaftLogError, RaftLogStore, StoredEntry};
use super::{Entry, LogId, TypeConfig, Vote};

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
            Some(bytes) => Some(serde_json::from_slice::<LogId>(&bytes)?),
            None => None,
        };
        let last_log_id = match self.store.last_entry()? {
            Some(entry) => Some(serde_json::from_slice::<Entry>(&entry.payload)?.log_id),
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
                index: entry.log_id.index,
                payload: serde_json::to_vec(&entry)?,
            });
        }
        self.store.append(&stored)?;
        Ok(())
    }

    /// Keeps entries through `last_log_id` and drops the rest; `None` empties the log.
    fn keep_through(&self, last_log_id: Option<LogId>) -> Result<(), LogStoreError> {
        self.store.truncate(last_log_id.map_or(0, |log_id| log_id.index + 1))?;
        Ok(())
    }

    /// Persists the full [`LogId`] watermark and purges through its index in one transaction.
    fn purge_upto(&self, log_id: LogId) -> Result<(), LogStoreError> {
        let marker = serde_json::to_vec(&log_id)?;
        self.store.purge(log_id.index, &marker)?;
        Ok(())
    }

    fn store_vote(&self, vote: &Vote) -> Result<(), LogStoreError> {
        self.store.save_vote(&serde_json::to_vec(vote)?)?;
        Ok(())
    }

    fn load_vote(&self) -> Result<Option<Vote>, LogStoreError> {
        match self.store.read_vote()? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            None => Ok(None),
        }
    }

    fn store_committed(&self, committed: Option<LogId>) -> Result<(), LogStoreError> {
        let bytes = match committed {
            Some(log_id) => Some(serde_json::to_vec(&log_id)?),
            None => None,
        };
        self.store.save_committed(bytes.as_deref())?;
        Ok(())
    }

    fn load_committed(&self) -> Result<Option<LogId>, LogStoreError> {
        match self.store.read_committed()? {
            Some(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            None => Ok(None),
        }
    }
}

impl RaftLogReader<TypeConfig> for RaftLogStoreAdapter {
    fn try_get_log_entries<RB: RangeBounds<u64> + Clone + Debug + OptionalSend>(
        &mut self,
        range: RB,
    ) -> impl Future<Output = Result<Vec<Entry>, io::Error>> + Send {
        std::future::ready(self.entries_in_range(range).map_err(io::Error::other))
    }

    fn read_vote(&mut self) -> impl Future<Output = Result<Option<Vote>, io::Error>> + Send {
        std::future::ready(self.load_vote().map_err(io::Error::other))
    }
}

// Every `io::Error` returned here is fatal to `OpenRaft`, which shuts the node down instead of retrying.
impl RaftLogStorage<TypeConfig> for RaftLogStoreAdapter {
    type LogReader = Self;

    fn get_log_state(&mut self) -> impl Future<Output = Result<LogState<TypeConfig>, io::Error>> + Send {
        std::future::ready(self.log_state().map_err(io::Error::other))
    }

    fn get_log_reader(&mut self) -> impl Future<Output = Self::LogReader> + Send {
        std::future::ready(self.clone())
    }

    fn save_vote(&mut self, vote: &Vote) -> impl Future<Output = Result<(), io::Error>> + Send {
        std::future::ready(self.store_vote(vote).map_err(io::Error::other))
    }

    fn append<I>(
        &mut self,
        entries: I,
        callback: IOFlushed<TypeConfig>,
    ) -> impl Future<Output = Result<(), io::Error>> + Send
    where
        I: IntoIterator<Item = Entry> + OptionalSend,
        I::IntoIter: OptionalSend,
    {
        std::future::ready(
            self.append_entries(entries)
                .map_err(io::Error::other)
                // redb committed the batch before this callback, satisfying OpenRaft's durability contract.
                .map(|()| callback.io_completed(Ok(()))),
        )
    }

    fn truncate_after(&mut self, last_log_id: Option<LogId>) -> impl Future<Output = Result<(), io::Error>> + Send {
        std::future::ready(self.keep_through(last_log_id).map_err(io::Error::other))
    }

    fn purge(&mut self, log_id: LogId) -> impl Future<Output = Result<(), io::Error>> + Send {
        std::future::ready(self.purge_upto(log_id).map_err(io::Error::other))
    }

    fn save_committed(&mut self, committed: Option<LogId>) -> impl Future<Output = Result<(), io::Error>> + Send {
        std::future::ready(self.store_committed(committed).map_err(io::Error::other))
    }

    fn read_committed(&mut self) -> impl Future<Output = Result<Option<LogId>, io::Error>> + Send {
        std::future::ready(self.load_committed().map_err(io::Error::other))
    }
}

#[cfg(test)]
#[path = "../../tests/unit/raft/log_store/tests.rs"]
mod tests;
