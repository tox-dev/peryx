use redb::ReadableTable as _;

use super::error::{MetaError, MetaScanError};
use super::{
    BLOB_RECLAIM_GUARD, DRIVER_KV, DriverBatch, DriverBlobReference, DriverMutation, JOURNAL, JOURNAL_BLOBS,
    JOURNAL_MUTATIONS, JournalEntry, MetaStore, POLICY_INPUT_GENERATION, PolicyInputGeneration, SERIAL, SERIAL_KEY,
};

impl MetaStore {
    /// Treats keys and values as opaque bytes.
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

    /// # Errors
    /// Returns a store error if the read fails.
    pub fn get_driver_value(&self, key: &str) -> Result<Option<Vec<u8>>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(DRIVER_KV)?;
        Ok(table.get(key)?.map(|value| value.value().to_vec()))
    }

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

    /// Commits the read, transformation, and write atomically.
    ///
    /// # Errors
    /// Returns a store error if the read, transformation, or write fails.
    pub fn update_driver_value<R>(
        &self,
        key: &str,
        update: impl FnOnce(Option<&[u8]>) -> Result<(Option<Vec<u8>>, R), MetaError>,
    ) -> Result<R, MetaError> {
        let txn = self.db.begin_write()?;
        let result;
        {
            let mut table = txn.open_table(DRIVER_KV)?;
            let current = table.get(key)?;
            let (next, updated) = update(current.as_ref().map(redb::AccessGuard::value))?;
            drop(current);
            match next {
                Some(value) => {
                    table.insert(key, value.as_slice())?;
                }
                None => {
                    table.remove(key)?;
                }
            }
            result = updated;
        }
        txn.commit()?;
        Ok(result)
    }

    /// Removes at most `limit` matching values accepted by `remove`.
    ///
    /// # Errors
    /// Returns a store error if scanning, classification, or deletion fails.
    pub fn remove_driver_values_if(
        &self,
        prefix: &str,
        limit: usize,
        mut remove: impl FnMut(&[u8]) -> Result<bool, MetaError>,
    ) -> Result<Vec<String>, MetaError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let txn = self.db.begin_write()?;
        let removed;
        {
            let mut table = txn.open_table(DRIVER_KV)?;
            let mut keys = Vec::new();
            for entry in table.range(prefix..)? {
                let (key, value) = entry?;
                if !key.value().starts_with(prefix) {
                    break;
                }
                if remove(value.value())? {
                    keys.push(key.value().to_owned());
                    if keys.len() == limit {
                        break;
                    }
                }
            }
            for key in &keys {
                table.remove(key.as_str())?;
            }
            removed = keys;
        }
        txn.commit()?;
        Ok(removed)
    }

    /// Returns matching keys in key order.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn driver_prefix_keys(&self, prefix: &str) -> Result<Vec<String>, MetaError> {
        self.driver_prefix_keys_limited(prefix, usize::MAX)
    }

    /// Returns at most `limit` matching keys in key order.
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

    /// Visits matching records in key order without collecting them.
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

    /// Runs dependent driver reads against one snapshot.
    ///
    /// # Errors
    /// Returns a store error if the snapshot cannot be opened, or the callback's error.
    pub fn read_driver_txn<T, E: From<MetaError>>(
        &self,
        body: impl FnOnce(&DriverReadTxn) -> Result<T, E>,
    ) -> Result<T, E> {
        let txn = self.db.begin_read().map_err(MetaError::from)?;
        let driver = DriverReadTxn {
            table: txn.open_table(DRIVER_KV).map_err(MetaError::from)?,
        };
        body(&driver)
    }

    /// Reads driver records and policy generation from one snapshot.
    ///
    /// # Errors
    /// Returns a scan error if the store read fails or either callback rejects the snapshot.
    pub fn visit_driver_policy_snapshot<E>(
        &self,
        prefix: &str,
        repository: &str,
        start: impl FnOnce(PolicyInputGeneration) -> Result<(), E>,
        mut visit: impl FnMut(&str, &[u8]) -> Result<(), E>,
    ) -> Result<(), MetaScanError<E>> {
        let txn = self.db.begin_read().map_err(MetaError::from)?;
        let mut generation = txn
            .open_table(POLICY_INPUT_GENERATION)
            .map_err(MetaError::from)?
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
        start(generation).map_err(MetaScanError::Visit)?;
        let table = txn.open_table(DRIVER_KV).map_err(MetaError::from)?;
        for entry in table.range(prefix..).map_err(MetaError::from)? {
            let (key, value) = entry.map_err(MetaError::from)?;
            if !key.value().starts_with(prefix) {
                break;
            }
            visit(key.value(), value.value()).map_err(MetaScanError::Visit)?;
        }
        Ok(())
    }

    /// Commits a driver batch atomically. `durable = false` skips fsync for data that callers can refetch
    /// after a crash.
    ///
    /// # Errors
    /// Returns a store error if the write or commit fails.
    ///
    /// # Panics
    /// Panics if redb rejects reduced durability despite this transaction having no savepoints.
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

    /// Keeps cache reads and writes atomic but skips fsync; a crash may discard them before a durable
    /// commit.
    ///
    /// # Errors
    /// Returns the body's error, or a store error mapped into it, if the transaction fails to open,
    /// read, write, or commit.
    ///
    /// # Panics
    /// Panics if redb rejects reduced durability despite this transaction having no savepoints.
    pub fn commit_driver_cache_txn<T, E: From<MetaError>>(
        &self,
        body: impl FnOnce(&mut DriverTxn) -> Result<T, E>,
    ) -> Result<T, E> {
        self.commit_driver_txn_at(
            None,
            None,
            false,
            |_, _| Ok(()),
            |txn| body(txn).map(|value| (value, Vec::new())),
        )
    }

    /// Commits checks, driver rows, and serial-ordered journal entries atomically. An empty journal list
    /// records no replicated change; a body error commits nothing. The final journal entry carries the
    /// transaction's final row values so page boundaries cannot expose partial changes.
    ///
    /// # Errors
    /// Returns the body's error, or a store error mapped into it, if the transaction fails to open,
    /// read, write, or commit.
    pub fn commit_driver_txn<T, E: From<MetaError>>(
        &self,
        body: impl FnOnce(&mut DriverTxn) -> Result<(T, Vec<Vec<u8>>), E>,
    ) -> Result<T, E> {
        self.commit_driver_txn_with_commit(body).map(|commit| commit.value)
    }

    /// # Errors
    /// Returns the body's error or a mapped store error when the transaction fails.
    pub fn commit_driver_txn_with_commit<T, E: From<MetaError>>(
        &self,
        body: impl FnOnce(&mut DriverTxn) -> Result<(T, Vec<Vec<u8>>), E>,
    ) -> Result<super::DriverCommit<T>, E> {
        self.commit_driver_txn_at_with_commit(None, None, true, |_, _| Ok(()), body)
    }

    /// Commits driver rows and their catalog generation atomically.
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
        self.commit_driver_txn_at(None, Some((repository, catalog_generation)), true, |_, _| Ok(()), body)
    }

    /// Commits replicated rows and serial-ordered journal entries only at `expected_serial`. Each entry
    /// retains its metadata mutations and blob references.
    ///
    /// # Errors
    /// Returns [`MetaError::ReplicaSerialConflict`] through `E` when the local serial differs, the
    /// body's error, or a store error if the transaction fails.
    pub fn commit_replica_txn<T, E: From<MetaError>>(
        &self,
        expected_serial: u64,
        body: impl FnOnce(&mut DriverTxn) -> Result<(T, Vec<JournalEntry>), E>,
    ) -> Result<T, E> {
        self.commit_driver_txn_with_journal(
            Some(expected_serial),
            None,
            true,
            |_, _| Ok(()),
            |txn| body(txn).map(|(value, entries)| (value, PendingJournal::Entries(entries))),
        )
        .map(|commit| commit.value)
    }

    pub(super) fn commit_driver_txn_at<T, E: From<MetaError>>(
        &self,
        expected_serial: Option<u64>,
        catalog_generation: Option<(&str, u64)>,
        durable: bool,
        finalize: impl FnOnce(&redb::WriteTransaction, &T) -> Result<(), E>,
        body: impl FnOnce(&mut DriverTxn) -> Result<(T, Vec<Vec<u8>>), E>,
    ) -> Result<T, E> {
        self.commit_driver_txn_at_with_commit(expected_serial, catalog_generation, durable, finalize, body)
            .map(|commit| commit.value)
    }

    pub(super) fn commit_driver_txn_at_with_commit<T, E: From<MetaError>>(
        &self,
        expected_serial: Option<u64>,
        catalog_generation: Option<(&str, u64)>,
        durable: bool,
        finalize: impl FnOnce(&redb::WriteTransaction, &T) -> Result<(), E>,
        body: impl FnOnce(&mut DriverTxn) -> Result<(T, Vec<Vec<u8>>), E>,
    ) -> Result<super::DriverCommit<T>, E> {
        self.commit_driver_txn_with_journal(expected_serial, catalog_generation, durable, finalize, |txn| {
            body(txn).map(|(value, entries)| (value, PendingJournal::Payloads(entries)))
        })
    }

    fn commit_driver_txn_with_journal<T, E: From<MetaError>>(
        &self,
        expected_serial: Option<u64>,
        catalog_generation: Option<(&str, u64)>,
        durable: bool,
        finalize: impl FnOnce(&redb::WriteTransaction, &T) -> Result<(), E>,
        body: impl FnOnce(&mut DriverTxn) -> Result<(T, PendingJournal), E>,
    ) -> Result<super::DriverCommit<T>, E> {
        let mut txn = self.db.begin_write().map_err(MetaError::from)?;
        set_durability(&mut txn, durable);
        check_replica_serial(&txn, expected_serial)?;
        let (value, journal) = {
            let mut driver = DriverTxn {
                table: txn.open_table(DRIVER_KV).map_err(MetaError::from)?,
                touched: std::collections::BTreeSet::new(),
                blobs: std::collections::BTreeSet::new(),
            };
            let (value, journal) = body(&mut driver)?;
            let journal = finish_journal(journal, driver).map_err(E::from)?;
            (value, journal)
        };
        check_blob_reclaim_guard(&txn, expected_serial, &journal).map_err(E::from)?;
        let journal_commit = commit_journal(&txn, &journal)?;
        if let Some((repository, catalog)) = catalog_generation {
            update_policy_generation(&txn, repository, catalog).map_err(E::from)?;
        }
        finalize(&txn, &value)?;
        txn.commit().map_err(MetaError::from)?;
        Ok(super::DriverCommit {
            value,
            journal: journal_commit,
        })
    }
}

enum PendingJournal {
    Payloads(Vec<Vec<u8>>),
    Entries(Vec<JournalEntry>),
}

fn set_durability(txn: &mut redb::WriteTransaction, durable: bool) {
    if !durable {
        txn.set_durability(redb::Durability::None)
            .expect("no savepoints in this transaction");
    }
}

fn finish_journal(journal: PendingJournal, driver: DriverTxn<'_>) -> Result<Vec<JournalEntry>, MetaError> {
    match journal {
        PendingJournal::Payloads(payloads) => {
            let mut entries: Vec<_> = payloads
                .into_iter()
                .map(|payload| JournalEntry {
                    payload,
                    mutations: Vec::new(),
                    blobs: Vec::new(),
                })
                .collect();
            if let Some(last) = entries.last_mut() {
                last.mutations = driver.mutations()?;
                last.blobs = driver.blobs.into_iter().collect();
            }
            Ok(entries)
        }
        PendingJournal::Entries(entries) => Ok(entries),
    }
}

fn check_blob_reclaim_guard(
    txn: &redb::WriteTransaction,
    expected_serial: Option<u64>,
    journal: &[JournalEntry],
) -> Result<(), MetaError> {
    let Some(last) = journal.last() else {
        return Ok(());
    };
    if expected_serial.is_some() || last.blobs.is_empty() {
        return Ok(());
    }
    let guards = txn.open_table(BLOB_RECLAIM_GUARD)?;
    for blob in &last.blobs {
        if guards.get(blob.sha256.as_str())?.is_some() {
            return Err(MetaError::BlobReclaiming {
                digest: blob.sha256.clone(),
            });
        }
    }
    Ok(())
}

fn update_policy_generation(txn: &redb::WriteTransaction, repository: &str, catalog: u64) -> Result<(), MetaError> {
    let mut generations = txn.open_table(POLICY_INPUT_GENERATION)?;
    let mut generation = generations
        .get(repository)?
        .map(|value| serde_json::from_slice::<PolicyInputGeneration>(value.value()))
        .transpose()?
        .unwrap_or_default();
    generation.repository = txn
        .open_table(SERIAL)?
        .get(SERIAL_KEY)?
        .map_or(0, |value| value.value());
    generation.catalog = catalog;
    let encoded = serde_json::to_vec(&generation)?;
    generations.insert(repository, encoded.as_slice())?;
    Ok(())
}

fn check_replica_serial<E: From<MetaError>>(txn: &redb::WriteTransaction, expected: Option<u64>) -> Result<(), E> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let serials = txn.open_table(SERIAL).map_err(MetaError::from)?;
    let actual = serials
        .get(SERIAL_KEY)
        .map_err(MetaError::from)?
        .map_or(0, |value| value.value());
    if actual != expected {
        return Err(MetaError::ReplicaSerialConflict { expected, actual }.into());
    }
    Ok(())
}

fn commit_journal<E: From<MetaError>>(
    txn: &redb::WriteTransaction,
    journal: &[JournalEntry],
) -> Result<Option<super::JournalCommit>, E> {
    if journal.is_empty() {
        return Ok(None);
    }
    let mut serials = txn.open_table(SERIAL).map_err(MetaError::from)?;
    let mut events = txn.open_table(JOURNAL).map_err(MetaError::from)?;
    let mut next = serials
        .get(SERIAL_KEY)
        .map_err(MetaError::from)?
        .map_or(0, |value| value.value());
    for entry in journal {
        next += 1;
        events.insert(next, entry.payload.as_slice()).map_err(MetaError::from)?;
        if !entry.mutations.is_empty() {
            let encoded = serde_json::to_vec(&entry.mutations).map_err(MetaError::from)?;
            txn.open_table(JOURNAL_MUTATIONS)
                .map_err(MetaError::from)?
                .insert(next, encoded.as_slice())
                .map_err(MetaError::from)?;
        }
        if !entry.blobs.is_empty() {
            let encoded = serde_json::to_vec(&entry.blobs).map_err(MetaError::from)?;
            txn.open_table(JOURNAL_BLOBS)
                .map_err(MetaError::from)?
                .insert(next, encoded.as_slice())
                .map_err(MetaError::from)?;
        }
    }
    serials.insert(SERIAL_KEY, next).map_err(MetaError::from)?;
    Ok(Some(super::JournalCommit::new(next)))
}

/// Gives a [`MetaStore::commit_driver_txn`] body atomic access to opaque driver rows.
pub struct DriverTxn<'txn> {
    table: redb::Table<'txn, &'static str, &'static [u8]>,
    touched: std::collections::BTreeSet<String>,
    blobs: std::collections::BTreeSet<DriverBlobReference>,
}

/// Read-only access to opaque driver rows from one metadata snapshot.
pub struct DriverReadTxn {
    table: redb::ReadOnlyTable<&'static str, &'static [u8]>,
}

/// Owned driver rows in key order.
pub type DriverEntries = Vec<(String, Vec<u8>)>;

impl DriverReadTxn {
    /// # Errors
    /// Returns a store error if the snapshot read fails.
    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>, MetaError> {
        Ok(self.table.get(key)?.map(|value| value.value().to_vec()))
    }

    /// Returns one group of matching records per prefix, preserving prefix and key order.
    ///
    /// # Errors
    /// Returns a store error if the snapshot scan fails.
    pub fn prefixes(&self, prefixes: &[&str]) -> Result<Vec<DriverEntries>, MetaError> {
        prefixes.iter().map(|prefix| self.collect_prefix(prefix)).collect()
    }

    /// Returns matching records in key order.
    ///
    /// # Errors
    /// Returns a store error if the snapshot scan fails.
    pub fn prefix(&self, prefix: &str) -> Result<DriverEntries, MetaError> {
        self.collect_prefix(prefix)
    }

    fn collect_prefix(&self, prefix: &str) -> Result<DriverEntries, MetaError> {
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
}

impl DriverTxn<'_> {
    /// Includes writes staged earlier in this transaction.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>, MetaError> {
        Ok(self.table.get(key)?.map(|value| value.value().to_vec()))
    }

    /// Returns matching rows in key order.
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

    /// # Errors
    /// Returns a store error if the write fails.
    pub fn put(&mut self, key: &str, value: &[u8]) -> Result<(), MetaError> {
        self.upsert(key, value)?;
        Ok(())
    }

    /// Excludes the value from replicated mutations.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    pub fn put_local(&mut self, key: &str, value: &[u8]) -> Result<(), MetaError> {
        self.table.insert(key, value)?;
        Ok(())
    }

    /// Adds no replicated mutation.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    pub fn remove_local(&mut self, key: &str) -> Result<bool, MetaError> {
        Ok(self.table.remove(key)?.is_some())
    }

    pub fn reference_blob(&mut self, sha256: &str, size: u64) {
        self.blobs.insert(DriverBlobReference {
            sha256: sha256.to_owned(),
            size,
        });
    }

    /// Returns whether the write inserted a new key.
    ///
    /// # Errors
    /// Returns a store error if the write fails.
    pub fn upsert(&mut self, key: &str, value: &[u8]) -> Result<bool, MetaError> {
        let inserted = self.table.insert(key, value)?.is_none();
        self.touched.insert(key.to_owned());
        Ok(inserted)
    }

    /// Returns whether `key` was present.
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
#[path = "../../tests/unit/meta/index/tests.rs"]
mod tests;
