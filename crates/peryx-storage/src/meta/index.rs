use redb::{ReadableDatabase as _, ReadableTable as _};

use super::error::MetaError;
use super::{DRIVER_KV, DriverBatch, JOURNAL, MetaStore, SERIAL, SERIAL_KEY};

impl MetaStore {
    /// Store a driver-owned value under `key`. The store treats both as opaque bytes.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    pub fn put_driver_value(&self, key: &str, value: &[u8]) -> Result<(), MetaError> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(DRIVER_KV)?;
            table.insert(key, value)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Fetch a driver-owned value by `key`.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn get_driver_value(&self, key: &str) -> Result<Option<Vec<u8>>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(DRIVER_KV)?;
        Ok(table.get(key)?.map(|value| value.value().to_vec()))
    }

    /// Remove a driver-owned value, reporting whether it was present.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    pub fn delete_driver_value(&self, key: &str) -> Result<bool, MetaError> {
        let txn = self.db.begin_write()?;
        let removed = {
            let mut table = txn.open_table(DRIVER_KV)?;
            table.remove(key)?.is_some()
        };
        txn.commit()?;
        Ok(removed)
    }

    /// Collect every driver-owned key that starts with `prefix`, in key order.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn driver_prefix_keys(&self, prefix: &str) -> Result<Vec<String>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(DRIVER_KV)?;
        let mut keys = Vec::new();
        for entry in table.range(prefix..)? {
            let (key, _) = entry?;
            if !key.value().starts_with(prefix) {
                break;
            }
            keys.push(key.value().to_owned());
        }
        Ok(keys)
    }

    /// Apply a batch of driver-owned writes in one transaction. `durable` requests an fsync-backed
    /// commit; pass `false` for re-fetchable cache data, where skipping the fsync keeps a large-page
    /// write at memory speed and a crash before the next durable commit only costs a refetch — the
    /// fast path a write per key would lose.
    ///
    /// # Errors
    /// Returns a store error if the write or commit fails.
    ///
    /// # Panics
    /// Never in practice: reducing durability is rejected only after savepoint use, and this
    /// transaction takes none.
    pub fn commit_driver_batch(&self, batch: &DriverBatch, durable: bool) -> Result<(), MetaError> {
        let mut txn = self.db.begin_write()?;
        if !durable {
            txn.set_durability(redb::Durability::None)
                .expect("no savepoints in this transaction");
        }
        {
            let mut table = txn.open_table(DRIVER_KV)?;
            for (key, value) in &batch.puts {
                table.insert(key.as_str(), value.as_slice())?;
            }
            for key in &batch.deletes {
                table.remove(key.as_str())?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// Run a driver-owned read-modify-write over the neutral table in one write transaction.
    ///
    /// A check and the writes it gates commit together, so neither can interleave with another
    /// writer. `body` reads current rows through the [`DriverTxn`], stages its puts and deletes, and
    /// returns the value to hand back paired with the journal entries to record: each allocates the
    /// next serial and is written in the same transaction, in order, so a batch that changes many
    /// files records one entry per file and a replica observes every one. An empty list commits the
    /// rows alone, for a change no replica reconciles. Returning an error from `body` drops the
    /// transaction, so a rejected precondition leaves the store untouched.
    ///
    /// # Errors
    /// Returns the body's error, or a store error mapped into it, if the transaction fails to open,
    /// read, write, or commit.
    pub fn commit_driver_txn<T, E: From<MetaError>>(
        &self,
        body: impl FnOnce(&mut DriverTxn) -> Result<(T, Vec<Vec<u8>>), E>,
    ) -> Result<T, E> {
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        let (value, journal) = {
            let mut driver = DriverTxn {
                table: txn.open_table(DRIVER_KV).map_err(MetaError::from)?,
            };
            body(&mut driver)?
        };
        if !journal.is_empty() {
            let mut serials = txn.open_table(SERIAL).map_err(MetaError::from)?;
            let mut journal_table = txn.open_table(JOURNAL).map_err(MetaError::from)?;
            let mut next = serials
                .get(SERIAL_KEY)
                .map_err(MetaError::from)?
                .map_or(0, |value| value.value());
            for entry in &journal {
                next += 1;
                journal_table.insert(next, entry.as_slice()).map_err(MetaError::from)?;
            }
            serials.insert(SERIAL_KEY, next).map_err(MetaError::from)?;
        }
        txn.commit().map_err(MetaError::from)?;
        Ok(value)
    }
}

/// A handle to the neutral key-value table inside an open write transaction.
///
/// Handed to a [`commit_driver_txn`](MetaStore::commit_driver_txn) body so it can read current rows
/// and stage writes atomically. Keys and values stay opaque bytes the store never interprets.
pub struct DriverTxn<'txn> {
    table: redb::Table<'txn, &'static str, &'static [u8]>,
}

impl DriverTxn<'_> {
    /// The current value of `key`, reflecting writes already staged in this transaction.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>, MetaError> {
        Ok(self.table.get(key)?.map(|value| value.value().to_vec()))
    }

    /// Every `(key, value)` whose key starts with `prefix`, in key order.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn prefix(&self, prefix: &str) -> Result<Vec<(String, Vec<u8>)>, MetaError> {
        let mut entries = Vec::new();
        for entry in self.table.range(prefix..)? {
            let (key, value) = entry?;
            if !key.value().starts_with(prefix) {
                break;
            }
            entries.push((key.value().to_owned(), value.value().to_vec()));
        }
        Ok(entries)
    }

    /// Stage an upsert of `key` to `value`.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    pub fn put(&mut self, key: &str, value: &[u8]) -> Result<(), MetaError> {
        self.table.insert(key, value)?;
        Ok(())
    }

    /// Stage a removal of `key`, reporting whether it was present.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    pub fn remove(&mut self, key: &str) -> Result<bool, MetaError> {
        Ok(self.table.remove(key)?.is_some())
    }
}
