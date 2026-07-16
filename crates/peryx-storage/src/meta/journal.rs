use redb::{ReadableDatabase as _, ReadableTable as _};
use std::ops::Bound::{Excluded, Unbounded};

use super::error::MetaError;
use serde::{Deserialize, Serialize};

use super::{JOURNAL, JOURNAL_MUTATIONS, MetaStore, SERIAL, SERIAL_KEY};

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
        let txn = self.db.begin_read()?;
        let current = txn
            .open_table(SERIAL)?
            .get(SERIAL_KEY)?
            .map_or(0, |value| value.value());
        let journal = txn.open_table(JOURNAL)?;
        let mutations = txn.open_table(JOURNAL_MUTATIONS)?;
        let records = journal
            .range((Excluded(serial), Unbounded))?
            .take(limit)
            .map(|entry| -> Result<JournalRecord, MetaError> {
                let (serial, payload) = entry?;
                let serial = serial.value();
                Ok(JournalRecord {
                    serial,
                    payload: payload.value().to_vec(),
                    mutations: mutations
                        .get(serial)?
                        .map(|value| serde_json::from_slice(value.value()))
                        .transpose()?
                        .unwrap_or_default(),
                })
            })
            .collect::<Result<_, _>>()?;
        Ok((current, records))
    }
}
