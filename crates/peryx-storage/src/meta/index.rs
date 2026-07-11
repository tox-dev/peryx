use redb::{ReadableDatabase as _, ReadableTable as _};

use super::error::{MetaError, MetaScanError};
use super::record::{CachedIndex, CachedIndexPage, ProjectStatusRecord};
use super::{
    DRIVER_KV, DriverBatch, FILE, INDEX, JOURNAL, METADATA, MetaStore, PROJECT_STATUS, PROJECTS, SERIAL, SERIAL_KEY,
    file_source_value, metadata_value,
};

impl MetaStore {
    /// Store everything a freshly fetched cached page produces in one transaction: the cached page
    /// record, the observed project name, every file's source URL, and every PEP 658 sibling.
    /// One commit means one fsync, where a write per file made large projects (numpy has thousands
    /// of files) take tens of seconds.
    ///
    /// # Errors
    /// Returns a store error if the write or commit fails.
    ///
    /// # Panics
    /// Never in practice: reducing durability is only rejected after savepoint use, and this
    /// transaction uses none.
    #[allow(
        clippy::too_many_arguments,
        reason = "one transaction needs every table's rows together"
    )]
    pub fn put_cached_page(
        &self,
        key: &str,
        record: &CachedIndex,
        index: &str,
        normalized: &str,
        display: &str,
        source: &str,
        project_status: Option<&str>,
        project_status_reason: Option<&str>,
        files: &[(String, String, Option<u64>)],
        metadata: &[(String, String, String)],
    ) -> Result<(), MetaError> {
        let bytes = record.encode();
        let project_key = format!("{index}/{normalized}");
        let mut txn = self.db.begin_write()?;
        // Page EOF waits on this commit so downloads always find their registrations; skipping the
        // fsync keeps that wait to memory speed. The rows are re-fetchable cache data: a crash
        // before the next durable commit only costs a refetch.
        txn.set_durability(redb::Durability::None)
            .expect("no savepoints in this transaction");
        {
            let mut table = txn.open_table(INDEX)?;
            table.insert(key, bytes.as_slice())?;
            let mut table = txn.open_table(PROJECTS)?;
            table.insert(project_key.as_str(), display)?;
            let mut table = txn.open_table(PROJECT_STATUS)?;
            match (project_status, project_status_reason) {
                (None, None) => {
                    table.remove(project_key.as_str())?;
                }
                (status, reason) => {
                    let record = serde_json::to_vec(&ProjectStatusRecord {
                        status: status.map(str::to_owned),
                        reason: reason.map(str::to_owned),
                    })?;
                    table.insert(project_key.as_str(), record.as_slice())?;
                }
            }
            let mut table = txn.open_table(FILE)?;
            for (sha256, url, size) in files {
                let value = file_source_value(url, source, *size);
                table.insert(sha256.as_str(), value.as_str())?;
            }
            let mut table = txn.open_table(METADATA)?;
            for (wheel_sha256, url, metadata_sha256) in metadata {
                let value = metadata_value(url, metadata_sha256, source);
                table.insert(wheel_sha256.as_str(), value.as_str())?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// Fetch one project's explicit status marker, if a cached upstream page provided one.
    ///
    /// # Errors
    /// Returns a store error if the read fails or the stored record cannot be decoded.
    pub fn get_project_status(&self, index: &str, normalized: &str) -> Result<Option<ProjectStatusRecord>, MetaError> {
        let key = format!("{index}/{normalized}");
        let txn = self.db.begin_read()?;
        let table = txn.open_table(PROJECT_STATUS)?;
        Ok(table
            .get(key.as_str())?
            .map(|value| serde_json::from_slice(value.value()))
            .transpose()?)
    }

    /// Store a cached index record under `key` (for example `root/pypi/flask`).
    ///
    /// # Errors
    /// Returns a store error if the write or commit fails.
    pub fn put_index(&self, key: &str, record: &CachedIndex) -> Result<(), MetaError> {
        let bytes = record.encode();
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(INDEX)?;
            table.insert(key, bytes.as_slice())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Fetch a cached index record.
    ///
    /// # Errors
    /// Returns a store error if the read fails or the stored bytes cannot be decoded.
    pub fn get_index(&self, key: &str) -> Result<Option<CachedIndex>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(INDEX)?;
        match table.get(key)? {
            Some(value) => Ok(Some(CachedIndex::decode(value.value())?)),
            None => Ok(None),
        }
    }

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

    /// Every cached page's key, fetch timestamp, and upstream freshness lifetime, for the
    /// background refresher to find stale entries without loading the (potentially multi-megabyte)
    /// bodies into a list.
    ///
    /// # Errors
    /// Returns a store error if the read fails or a stored record cannot be decoded.
    pub fn list_index_pages(&self) -> Result<Vec<(String, i64, Option<i64>)>, MetaError> {
        let mut pages = Vec::new();
        let txn = self.db.begin_read()?;
        let table = txn.open_table(INDEX)?;
        for entry in table.iter()? {
            let (key, value) = entry?;
            let (fetched_at, fresh_secs) = CachedIndex::decode_freshness(value.value())?;
            pages.push((key.value().to_owned(), fetched_at, fresh_secs));
        }
        Ok(pages)
    }

    /// Visit cached simple-index page summaries without collecting the table.
    ///
    /// # Errors
    /// Returns a scan error if the store read fails, a record cannot be decoded, or the visitor
    /// returns an error.
    pub fn scan_index_pages<E>(
        &self,
        mut visit: impl FnMut(CachedIndexPage) -> Result<(), E>,
    ) -> Result<(), MetaScanError<E>> {
        let txn = self.db.begin_read().map_err(MetaError::from)?;
        let table = txn.open_table(INDEX).map_err(MetaError::from)?;
        for entry in table.iter().map_err(MetaError::from)? {
            let (key, value) = entry.map_err(MetaError::from)?;
            visit(CachedIndexPage {
                key: key.value().to_owned(),
                summary: CachedIndex::summary(value.value()).map_err(MetaError::from)?,
            })
            .map_err(MetaScanError::Visit)?;
        }
        Ok(())
    }

    /// Visit raw cached simple-index records.
    ///
    /// # Errors
    /// Returns a scan error if the store read fails or the visitor returns an error.
    pub fn scan_index_records<E>(
        &self,
        mut visit: impl FnMut(&str, &[u8]) -> Result<(), E>,
    ) -> Result<(), MetaScanError<E>> {
        let txn = self.db.begin_read().map_err(MetaError::from)?;
        let table = txn.open_table(INDEX).map_err(MetaError::from)?;
        for entry in table.iter().map_err(MetaError::from)? {
            let (key, value) = entry.map_err(MetaError::from)?;
            visit(key.value(), value.value()).map_err(MetaScanError::Visit)?;
        }
        Ok(())
    }
}
