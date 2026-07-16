use std::ops::Bound::{Excluded, Unbounded};

use redb::{ReadableDatabase as _, ReadableTable as _};
use serde::{Deserialize, Serialize};

use super::error::MetaError;
use super::{JOURNAL, JOURNAL_BLOBS, JOURNAL_MUTATIONS, MetaStore, SERIAL, SERIAL_KEY};

/// One content blob required by a journal transaction.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DriverBlobReference {
    pub sha256: String,
    pub size: u64,
}

/// One final driver row change committed with a journal transaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "kebab-case")]
pub enum DriverMutation {
    Put { key: String, value: Vec<u8> },
    Delete { key: String },
}

/// One journal payload paired with its authoritative serial and row changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalRecord {
    pub serial: u64,
    pub payload: Vec<u8>,
    pub mutations: Vec<DriverMutation>,
    pub blobs: Vec<DriverBlobReference>,
}

/// A bounded journal read and the head serial from the same database snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalSnapshot {
    pub current_serial: u64,
    pub records: Vec<JournalRecord>,
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

    /// Read at most `limit` journal values after `after` and the head serial from one snapshot.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn journal_snapshot(&self, after: u64, limit: usize) -> Result<JournalSnapshot, MetaError> {
        let txn = self.db.begin_read()?;
        let current_serial = txn
            .open_table(SERIAL)?
            .get(SERIAL_KEY)?
            .map_or(0, |value| value.value());
        let table = match txn.open_table(JOURNAL) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Ok(JournalSnapshot {
                    current_serial,
                    records: Vec::new(),
                });
            }
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
        let records = table
            .range((Excluded(after), Unbounded))?
            .take(limit)
            .map(|entry| -> Result<JournalRecord, MetaError> {
                let (serial, payload) = entry?;
                let serial = serial.value();
                Ok(JournalRecord {
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
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(JournalSnapshot {
            current_serial,
            records,
        })
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

    /// Read at most `limit` journal records after `serial`, in serial order.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn journal_after(&self, serial: u64, limit: usize) -> Result<Vec<JournalRecord>, MetaError> {
        self.journal_page_after(serial, limit).map(|(_, records)| records)
    }

    /// Read the current serial and at most `limit` later journal records from one snapshot.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn journal_page_after(&self, serial: u64, limit: usize) -> Result<(u64, Vec<JournalRecord>), MetaError> {
        let snapshot = self.journal_snapshot(serial, limit)?;
        Ok((snapshot.current_serial, snapshot.records))
    }
}
