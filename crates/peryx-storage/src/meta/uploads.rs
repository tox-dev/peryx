use redb::{ReadableDatabase as _, ReadableTable as _};

use super::error::{MetaError, MetaScanError};
use super::{MetaStore, OVERRIDE, PROJECTS, UPLOAD};

impl MetaStore {
    /// Store an uploaded file's serialized record on a private index, keyed by
    /// `{index}/{normalized}/{filename}` so each file is an independent entry (no read-modify-write
    /// race between concurrent uploads).
    ///
    /// # Errors
    /// Returns a store error if the write or commit fails.
    pub fn put_upload(&self, index: &str, normalized: &str, filename: &str, record: &[u8]) -> Result<(), MetaError> {
        let key = format!("{index}/{normalized}/{filename}");
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(UPLOAD)?;
            table.insert(key.as_str(), record)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Store promoted upload records and the observed project display name in one transaction.
    ///
    /// # Errors
    /// Returns a store error if the write or commit fails.
    pub fn put_uploads(
        &self,
        index: &str,
        normalized: &str,
        display: &str,
        records: &[(String, Vec<u8>)],
    ) -> Result<(), MetaError> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(UPLOAD)?;
            for (filename, record) in records {
                let key = format!("{index}/{normalized}/{filename}");
                table.insert(key.as_str(), record.as_slice())?;
            }
            let mut table = txn.open_table(PROJECTS)?;
            let key = format!("{index}/{normalized}");
            table.insert(key.as_str(), display)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Fetch one uploaded file record.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn get_upload(&self, index: &str, normalized: &str, filename: &str) -> Result<Option<Vec<u8>>, MetaError> {
        let key = format!("{index}/{normalized}/{filename}");
        let txn = self.db.begin_read()?;
        let table = txn.open_table(UPLOAD)?;
        Ok(table.get(key.as_str())?.map(|value| value.value().to_vec()))
    }

    /// List the `(filename, record)` pairs uploaded for `normalized` on `index`, sorted by filename.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn list_upload_entries(&self, index: &str, normalized: &str) -> Result<Vec<(String, Vec<u8>)>, MetaError> {
        let prefix = format!("{index}/{normalized}/");
        let txn = self.db.begin_read()?;
        let table = txn.open_table(UPLOAD)?;
        let mut entries = Vec::new();
        for entry in table.iter()? {
            let (key, value) = entry?;
            if let Some(filename) = key.value().strip_prefix(&prefix) {
                entries.push((filename.to_owned(), value.value().to_vec()));
            }
        }
        Ok(entries)
    }

    /// Delete one uploaded file record, returning whether it existed.
    ///
    /// # Errors
    /// Returns a store error if the write or commit fails.
    pub fn delete_upload(&self, index: &str, normalized: &str, filename: &str) -> Result<bool, MetaError> {
        let key = format!("{index}/{normalized}/{filename}");
        let txn = self.db.begin_write()?;
        let existed = {
            let mut table = txn.open_table(UPLOAD)?;
            table.remove(key.as_str())?.is_some()
        };
        txn.commit()?;
        Ok(existed)
    }

    /// Visit raw upload records.
    ///
    /// # Errors
    /// Returns a scan error if the store read fails or the visitor returns an error.
    pub fn scan_upload_records<E>(
        &self,
        mut visit: impl FnMut(&str, &[u8]) -> Result<(), E>,
    ) -> Result<(), MetaScanError<E>> {
        let txn = self.db.begin_read().map_err(MetaError::from)?;
        let table = txn.open_table(UPLOAD).map_err(MetaError::from)?;
        for entry in table.iter().map_err(MetaError::from)? {
            let (key, value) = entry.map_err(MetaError::from)?;
            visit(key.value(), value.value()).map_err(MetaScanError::Visit)?;
        }
        Ok(())
    }

    /// Record an override for a file served from a read-only layer: `kind` is `yanked` or
    /// `hidden`. Keyed like uploads, by `{index}/{normalized}/{filename}`.
    ///
    /// # Errors
    /// Returns a store error if the write or commit fails.
    pub fn put_override(&self, index: &str, normalized: &str, filename: &str, kind: &str) -> Result<(), MetaError> {
        let key = format!("{index}/{normalized}/{filename}");
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(OVERRIDE)?;
            table.insert(key.as_str(), kind)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Remove a file's override, returning whether one existed.
    ///
    /// # Errors
    /// Returns a store error if the write or commit fails.
    pub fn delete_override(&self, index: &str, normalized: &str, filename: &str) -> Result<bool, MetaError> {
        let key = format!("{index}/{normalized}/{filename}");
        let txn = self.db.begin_write()?;
        let existed = {
            let mut table = txn.open_table(OVERRIDE)?;
            table.remove(key.as_str())?.is_some()
        };
        txn.commit()?;
        Ok(existed)
    }

    /// List the `(filename, kind)` overrides recorded for `normalized` on `index`.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn list_overrides(&self, index: &str, normalized: &str) -> Result<Vec<(String, String)>, MetaError> {
        let prefix = format!("{index}/{normalized}/");
        let txn = self.db.begin_read()?;
        let table = txn.open_table(OVERRIDE)?;
        let mut entries = Vec::new();
        for entry in table.iter()? {
            let (key, value) = entry?;
            if let Some(filename) = key.value().strip_prefix(&prefix) {
                entries.push((filename.to_owned(), value.value().to_owned()));
            }
        }
        Ok(entries)
    }

    /// Visit raw override records.
    ///
    /// # Errors
    /// Returns a scan error if the store read fails or the visitor returns an error.
    pub fn scan_override_records<E>(
        &self,
        mut visit: impl FnMut(&str, &str) -> Result<(), E>,
    ) -> Result<(), MetaScanError<E>> {
        let txn = self.db.begin_read().map_err(MetaError::from)?;
        let table = txn.open_table(OVERRIDE).map_err(MetaError::from)?;
        for entry in table.iter().map_err(MetaError::from)? {
            let (key, value) = entry.map_err(MetaError::from)?;
            visit(key.value(), value.value()).map_err(MetaScanError::Visit)?;
        }
        Ok(())
    }
}
