use std::ops::Bound::{Excluded, Unbounded};
use std::ops::ControlFlow;

use redb::ReadableTable as _;
use serde::{Deserialize, Serialize};

pub use peryx_core::JournalCommit;

use super::error::MetaError;
use super::{JOURNAL, JOURNAL_BLOBS, JOURNAL_MUTATIONS, MetaStore, SERIAL, SERIAL_KEY};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverCommit<T> {
    pub value: T,
    pub journal: Option<JournalCommit>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DriverBlobReference {
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "kebab-case")]
pub enum DriverMutation {
    Put { key: String, value: Vec<u8> },
    Delete { key: String },
}

impl DriverMutation {
    /// The row this mutation names, whichever way it changed it.
    #[must_use]
    pub fn key(&self) -> &str {
        match self {
            Self::Put { key, .. } | Self::Delete { key } => key,
        }
    }
}

/// One appended record: an opaque `payload` and the rows and blobs it describes.
///
/// Every writer shares this log, so a `payload` is a JSON object carrying a tag key that names the
/// vocabulary that wrote it. A reader takes the records carrying its own tag, passes over the rest, and
/// treats a payload carrying its tag that it cannot decode as an error. Naming its own records rather
/// than recognizing foreign ones is what makes that decision total: a vocabulary added later needs no
/// cooperation from the readers that came before it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    pub payload: Vec<u8>,
    pub mutations: Vec<DriverMutation>,
    pub blobs: Vec<DriverBlobReference>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalRecord {
    pub serial: u64,
    pub payload: Vec<u8>,
    pub mutations: Vec<DriverMutation>,
    pub blobs: Vec<DriverBlobReference>,
}

/// Keeps the page and head serial consistent by reading both from one snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalSnapshot {
    pub current_serial: u64,
    pub records: Vec<JournalRecord>,
}

impl MetaStore {
    /// Returns `0` before the first write.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn current_serial(&self) -> Result<u64, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(SERIAL)?;
        Ok(table.get(SERIAL_KEY)?.map_or(0, |value| value.value()))
    }

    /// Decodes at most `limit` records after `after` one at a time and returns the head serial read
    /// from the same snapshot.
    ///
    /// `visit` returns [`ControlFlow::Break`] to stop before the next record is decoded, which lets a
    /// caller cap the work it pays for without holding the whole page in memory first. The head serial
    /// describes the journal rather than the delivered records, so a stopped walk still reports a page
    /// that is a prefix of the snapshot.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn visit_journal_page(
        &self,
        after: u64,
        limit: usize,
        mut visit: impl FnMut(JournalRecord) -> ControlFlow<()>,
    ) -> Result<u64, MetaError> {
        let txn = self.db.begin_read()?;
        let current_serial = txn
            .open_table(SERIAL)?
            .get(SERIAL_KEY)?
            .map_or(0, |value| value.value());
        let table = match txn.open_table(JOURNAL) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(current_serial),
            Err(error) => return Err(error.into()),
        };
        let mutations = match txn.open_table(JOURNAL_MUTATIONS) {
            Ok(table) => Some(table),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(error) => return Err(error.into()),
        };
        let blobs = match txn.open_table(JOURNAL_BLOBS) {
            Ok(table) => Some(table),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(error) => return Err(error.into()),
        };
        for entry in table.range((Excluded(after), Unbounded))?.take(limit) {
            let (serial, payload) = entry?;
            let serial = serial.value();
            let record = JournalRecord {
                serial,
                payload: payload.value().to_vec(),
                mutations: mutations
                    .as_ref()
                    .and_then(|table| table.get(serial).transpose())
                    .transpose()?
                    .map(|value| serde_json::from_slice(value.value()))
                    .transpose()?
                    .unwrap_or_default(),
                blobs: blobs
                    .as_ref()
                    .and_then(|table| table.get(serial).transpose())
                    .transpose()?
                    .map(|value| serde_json::from_slice(value.value()))
                    .transpose()?
                    .unwrap_or_default(),
            };
            if visit(record).is_break() {
                break;
            }
        }
        Ok(current_serial)
    }

    /// The lowest serial the journal still holds, or `None` when it holds nothing.
    ///
    /// A reader whose cursor sits below this has lost the records it would need to catch up, which is
    /// what makes a checkpoint the only way forward for it. Reading what the journal holds rather than a
    /// recorded floor keeps this true the moment retention starts removing rows, and true today, when
    /// nothing removes any.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn journal_floor(&self) -> Result<Option<u64>, MetaError> {
        let txn = self.db.begin_read()?;
        let Some(table) = super::open_optional_table(&txn, JOURNAL)? else {
            return Ok(None);
        };
        Ok(table.first()?.map(|(serial, _)| serial.value()))
    }

    /// Reads at most `limit` values after `after` with the head serial from the same snapshot.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn journal_snapshot(&self, after: u64, limit: usize) -> Result<JournalSnapshot, MetaError> {
        let mut records = Vec::new();
        let current_serial = self.visit_journal_page(after, limit, |record| {
            records.push(record);
            ControlFlow::Continue(())
        })?;
        Ok(JournalSnapshot {
            current_serial,
            records,
        })
    }

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

    /// Returns at most `limit` records after `serial`, in serial order.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn journal_after(&self, serial: u64, limit: usize) -> Result<Vec<JournalRecord>, MetaError> {
        self.journal_page_after(serial, limit).map(|(_, records)| records)
    }

    /// Returns the current serial and at most `limit` later records from one snapshot.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn journal_page_after(&self, serial: u64, limit: usize) -> Result<(u64, Vec<JournalRecord>), MetaError> {
        let snapshot = self.journal_snapshot(serial, limit)?;
        Ok((snapshot.current_serial, snapshot.records))
    }
}
