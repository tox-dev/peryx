use redb::{ReadableDatabase as _, ReadableTable as _};

use super::error::MetaError;
use super::{
    DRIVER_KV, DriverBatch, DriverBlobReference, DriverMutation, JOURNAL, JOURNAL_BLOBS, JOURNAL_MUTATIONS, MetaStore,
    POLICY_INPUT_GENERATION, PolicyInputGeneration, SERIAL, SERIAL_KEY,
};

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
        self.driver_prefix_keys_limited(prefix, usize::MAX)
    }

    /// Collect at most `limit` driver-owned keys that start with `prefix`, in key order.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn driver_prefix_keys_limited(&self, prefix: &str, limit: usize) -> Result<Vec<String>, MetaError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let txn = self.db.begin_read()?;
        let table = txn.open_table(DRIVER_KV)?;
        let mut keys = Vec::new();
        for entry in table.range(prefix..)? {
            let (key, _) = entry?;
            if !key.value().starts_with(prefix) {
                break;
            }
            keys.push(key.value().to_owned());
            if keys.len() == limit {
                break;
            }
        }
        Ok(keys)
    }

    /// Visit driver-owned records that start with `prefix`, in key order, without collecting them.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn visit_driver_prefix(&self, prefix: &str, mut visit: impl FnMut(&str, &[u8])) -> Result<(), MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(DRIVER_KV)?;
        for entry in table.range(prefix..)? {
            let (key, value) = entry?;
            if !key.value().starts_with(prefix) {
                break;
            }
            visit(key.value(), value.value());
        }
        Ok(())
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
    /// transaction, so a rejected precondition leaves the store untouched. The final journal entry
    /// carries the transaction's final row values, so a page boundary cannot expose partial row
    /// changes from a multi-entry commit.
    ///
    /// # Errors
    /// Returns the body's error, or a store error mapped into it, if the transaction fails to open,
    /// read, write, or commit.
    pub fn commit_driver_txn<T, E: From<MetaError>>(
        &self,
        body: impl FnOnce(&mut DriverTxn) -> Result<(T, Vec<Vec<u8>>), E>,
    ) -> Result<T, E> {
        self.commit_driver_txn_at(None, None, body)
    }

    /// Commit driver rows and publish the catalog generation that produced their policy inputs in
    /// the same transaction.
    ///
    /// # Errors
    /// Returns the body's error, or a store error mapped into it, if the transaction cannot be read,
    /// encoded, or committed.
    pub fn commit_driver_txn_with_catalog_generation<T, E: From<MetaError>>(
        &self,
        repository: &str,
        catalog_generation: u64,
        body: impl FnOnce(&mut DriverTxn) -> Result<(T, Vec<Vec<u8>>), E>,
    ) -> Result<T, E> {
        self.commit_driver_txn_at(None, Some((repository, catalog_generation)), body)
    }

    /// Apply replicated driver rows and copied journal entries only when the local serial matches
    /// `expected_serial`.
    ///
    /// The rows, journal, and serial commit together. A stale follower cannot apply the same page
    /// twice or skip over local state changed by another writer.
    ///
    /// # Errors
    /// Returns [`MetaError::ReplicaSerialConflict`] through `E` when the local serial differs, the
    /// body's error, or a store error if the transaction fails.
    pub fn commit_replica_txn<T, E: From<MetaError>>(
        &self,
        expected_serial: u64,
        body: impl FnOnce(&mut DriverTxn) -> Result<(T, Vec<Vec<u8>>), E>,
    ) -> Result<T, E> {
        self.commit_driver_txn_at(Some(expected_serial), None, body)
    }

    fn commit_driver_txn_at<T, E: From<MetaError>>(
        &self,
        expected_serial: Option<u64>,
        catalog_generation: Option<(&str, u64)>,
        body: impl FnOnce(&mut DriverTxn) -> Result<(T, Vec<Vec<u8>>), E>,
    ) -> Result<T, E> {
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        if let Some(expected) = expected_serial {
            let serials = txn.open_table(SERIAL).map_err(MetaError::from)?;
            let actual = serials
                .get(SERIAL_KEY)
                .map_err(MetaError::from)?
                .map_or(0, |value| value.value());
            if actual != expected {
                return Err(MetaError::ReplicaSerialConflict { expected, actual }.into());
            }
        }
        let (value, journal, mutations, blobs) = {
            let mut driver = DriverTxn {
                table: txn.open_table(DRIVER_KV).map_err(MetaError::from)?,
                touched: std::collections::BTreeSet::new(),
                blobs: std::collections::BTreeSet::new(),
            };
            let (value, journal) = body(&mut driver)?;
            let mutations = if journal.is_empty() {
                Vec::new()
            } else {
                driver.mutations().map_err(E::from)?
            };
            let blobs = if journal.is_empty() {
                Vec::new()
            } else {
                driver.blobs.into_iter().collect()
            };
            (value, journal, mutations, blobs)
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
            if !mutations.is_empty() {
                let encoded = serde_json::to_vec(&mutations).map_err(MetaError::from)?;
                txn.open_table(JOURNAL_MUTATIONS)
                    .map_err(MetaError::from)?
                    .insert(next, encoded.as_slice())
                    .map_err(MetaError::from)?;
            }
            if !blobs.is_empty() {
                let encoded = serde_json::to_vec(&blobs).map_err(MetaError::from)?;
                txn.open_table(JOURNAL_BLOBS)
                    .map_err(MetaError::from)?
                    .insert(next, encoded.as_slice())
                    .map_err(MetaError::from)?;
            }
            serials.insert(SERIAL_KEY, next).map_err(MetaError::from)?;
        }
        if let Some((repository, catalog)) = catalog_generation {
            let mut generations = txn.open_table(POLICY_INPUT_GENERATION).map_err(MetaError::from)?;
            let mut generation = generations
                .get(repository)
                .map_err(MetaError::from)?
                .map(|value| serde_json::from_slice::<PolicyInputGeneration>(value.value()))
                .transpose()
                .map_err(MetaError::from)?
                .unwrap_or_default();
            generation.repository = txn
                .open_table(SERIAL)
                .map_err(MetaError::from)?
                .get(SERIAL_KEY)
                .map_err(MetaError::from)?
                .map_or(0, |value| value.value());
            generation.catalog = catalog;
            let encoded = serde_json::to_vec(&generation).map_err(MetaError::from)?;
            generations
                .insert(repository, encoded.as_slice())
                .map_err(MetaError::from)?;
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
    touched: std::collections::BTreeSet<String>,
    blobs: std::collections::BTreeSet<DriverBlobReference>,
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

    /// Avoid copying values when a transaction needs only matching keys.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn prefix_keys(&self, prefix: &str) -> Result<Vec<String>, MetaError> {
        let mut keys = Vec::new();
        for entry in self.table.range(prefix..)? {
            let (key, _) = entry?;
            if !key.value().starts_with(prefix) {
                break;
            }
            keys.push(key.value().to_owned());
        }
        Ok(keys)
    }

    /// Stage an upsert of `key` to `value`.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    pub fn put(&mut self, key: &str, value: &[u8]) -> Result<(), MetaError> {
        self.upsert(key, value)?;
        Ok(())
    }

    /// Stage a process-local value that replicas must not copy.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    pub fn put_local(&mut self, key: &str, value: &[u8]) -> Result<(), MetaError> {
        self.table.insert(key, value)?;
        Ok(())
    }

    /// Record content bytes that replicas need before committing this transaction.
    pub fn reference_blob(&mut self, sha256: &str, size: u64) {
        self.blobs.insert(DriverBlobReference {
            sha256: sha256.to_owned(),
            size,
        });
    }

    /// Preserve insert-versus-replace information while staging a write.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    pub fn upsert(&mut self, key: &str, value: &[u8]) -> Result<bool, MetaError> {
        let inserted = self.table.insert(key, value)?.is_none();
        self.touched.insert(key.to_owned());
        Ok(inserted)
    }

    /// Stage a removal of `key`, reporting whether it was present.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    pub fn remove(&mut self, key: &str) -> Result<bool, MetaError> {
        let removed = self.table.remove(key)?.is_some();
        if removed {
            self.touched.insert(key.to_owned());
        }
        Ok(removed)
    }

    fn mutations(&self) -> Result<Vec<DriverMutation>, MetaError> {
        self.touched
            .iter()
            .map(|key| {
                Ok(self.get(key)?.map_or_else(
                    || DriverMutation::Delete { key: key.clone() },
                    |value| DriverMutation::Put {
                        key: key.clone(),
                        value,
                    },
                ))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::MetaStore;

    #[test]
    fn test_driver_prefix_keys_limited_bounds_results() {
        let dir = tempfile::tempdir().unwrap();
        let meta = MetaStore::open(dir.path().join("peryx.redb")).unwrap();
        for key in ["catalog/1", "catalog/2", "catalog/3", "other/1"] {
            meta.put_driver_value(key, b"value").unwrap();
        }

        assert!(meta.driver_prefix_keys_limited("catalog/", 0).unwrap().is_empty());
        assert_eq!(
            meta.driver_prefix_keys_limited("catalog/", 2).unwrap(),
            vec!["catalog/1", "catalog/2"]
        );
        assert_eq!(meta.driver_prefix_keys("other/").unwrap(), vec!["other/1"]);
        let mut visited = Vec::new();
        meta.visit_driver_prefix("catalog/", |key, value| {
            visited.push((key.to_owned(), value.to_vec()));
        })
        .unwrap();
        assert_eq!(visited.len(), 3);
    }
}
