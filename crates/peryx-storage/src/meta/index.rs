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

    /// Apply a driver-owned batch and, in the same transaction, allocate the next global serial and
    /// record `journal` (opaque bytes the driver owns) under it, returning the serial. A hosted
    /// publish keeps its rows, its journal entry, and its serial atomic: a row durable without its
    /// journal entry would serve forever yet never reach a replica.
    ///
    /// # Errors
    /// Returns a store error if the write or commit fails.
    pub fn commit_driver_batch_journaled(&self, batch: &DriverBatch, journal: &[u8]) -> Result<u64, MetaError> {
        let txn = self.db.begin_write()?;
        let serial = {
            let mut serials = txn.open_table(SERIAL)?;
            let next = serials.get(SERIAL_KEY)?.map_or(0, |value| value.value()) + 1;
            serials.insert(SERIAL_KEY, next)?;
            let mut journal_table = txn.open_table(JOURNAL)?;
            journal_table.insert(next, journal)?;
            let mut table = txn.open_table(DRIVER_KV)?;
            for (key, value) in &batch.puts {
                table.insert(key.as_str(), value.as_slice())?;
            }
            for key in &batch.deletes {
                table.remove(key.as_str())?;
            }
            next
        };
        txn.commit()?;
        Ok(serial)
    }
}
