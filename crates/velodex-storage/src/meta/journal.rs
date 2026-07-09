use redb::{ReadableDatabase as _, ReadableTable as _};
use serde::{Deserialize, Serialize};

use super::error::MetaError;
use super::{JOURNAL, MetaStore, SERIAL, SERIAL_KEY};

/// One recorded mutation in the [`MetaStore`] journal: the append-only changelog that makes velodex
/// an origin others can replicate from. `serial` orders entries; the rest names what changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub serial: u64,
    pub action: String,
    pub project: String,
    pub version: Option<String>,
    pub filename: Option<String>,
}

impl MetaStore {
    /// The current serial (0 before any write).
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn current_serial(&self) -> Result<u64, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(SERIAL)?;
        Ok(table.get(SERIAL_KEY)?.map_or(0, |value| value.value()))
    }

    /// Increment the serial and return the new value.
    ///
    /// # Errors
    /// Returns a store error if the write or commit fails.
    pub fn next_serial(&self) -> Result<u64, MetaError> {
        let txn = self.db.begin_write()?;
        let next = {
            let mut table = txn.open_table(SERIAL)?;
            let next = table.get(SERIAL_KEY)?.map_or(0, |value| value.value()) + 1;
            table.insert(SERIAL_KEY, next)?;
            next
        };
        txn.commit()?;
        Ok(next)
    }

    /// Append a mutation to the journal and return its serial.
    ///
    /// The serial is allocated and the entry recorded in one write transaction, so under redb's single
    /// writer the journal is an append-only log whose serials are monotonic in commit order: the
    /// changelog a replica or downstream replica replays. (Warehouse needs a Postgres advisory lock for
    /// this guarantee; redb gives it for free.) Timestamps join the entry when replication lands.
    ///
    /// # Errors
    /// Returns [`MetaError`] on a storage or encode failure.
    pub fn append_journal(
        &self,
        action: &str,
        project: &str,
        version: Option<&str>,
        filename: Option<&str>,
    ) -> Result<u64, MetaError> {
        let txn = self.db.begin_write()?;
        let serial = {
            let mut serials = txn.open_table(SERIAL)?;
            let next = serials.get(SERIAL_KEY)?.map_or(0, |value| value.value()) + 1;
            serials.insert(SERIAL_KEY, next)?;
            let entry = JournalEntry {
                serial: next,
                action: action.to_owned(),
                project: project.to_owned(),
                version: version.map(str::to_owned),
                filename: filename.map(str::to_owned),
            };
            let mut journal = txn.open_table(JOURNAL)?;
            journal.insert(next, serde_json::to_vec(&entry)?.as_slice())?;
            next
        };
        txn.commit()?;
        Ok(serial)
    }

    /// The journal entries after serial `after`, in serial order: the changelog since a point.
    ///
    /// # Errors
    /// Returns [`MetaError`] on a storage or decode failure.
    pub fn journal_since(&self, after: u64) -> Result<Vec<JournalEntry>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(JOURNAL)?;
        let mut entries = Vec::new();
        for row in table.range(after.saturating_add(1)..)? {
            let (_, value) = row?;
            entries.push(serde_json::from_slice(value.value())?);
        }
        Ok(entries)
    }
}
