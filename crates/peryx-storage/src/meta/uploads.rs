use redb::{ReadableDatabase as _, ReadableTable as _};

use super::error::{MetaError, MetaScanError};
use super::journal::append_in_txn;
use super::{METADATA, MetaStore, OVERRIDE, PROJECTS, UPLOAD, metadata_value};

/// The PEP 658 metadata sibling recorded alongside a published file.
pub struct MetadataSibling<'a> {
    /// The artifact's own sha256, which keys the row.
    pub artifact_sha256: &'a str,
    /// Where the sibling came from; `uploaded` for a file published here.
    pub url: &'a str,
    /// The sibling's sha256, so a later fetch can verify it.
    pub metadata_sha256: &'a str,
    /// The index that owns it.
    pub source: &'a str,
}

/// Everything one published file writes to the store.
pub struct PublishedFile<'a> {
    /// The hosted index the file lands on.
    pub index: &'a str,
    /// The project's normalized name, which keys its rows.
    pub normalized: &'a str,
    /// The project's display name, as the uploader spelled it.
    pub display: &'a str,
    /// The distribution filename.
    pub filename: &'a str,
    /// The serialized file record served on the project's page.
    pub record: &'a [u8],
    /// The release the file belongs to, recorded in the journal entry.
    pub version: &'a str,
    /// The file's metadata sibling, when it has one.
    pub metadata: Option<MetadataSibling<'a>>,
}

impl MetaStore {
    /// Publish a file: its metadata sibling, its record, its project, and its journal entry, together.
    ///
    /// One transaction, because these four rows are one fact. Committed separately, a crash between
    /// the upload row and the journal entry leaves peryx serving a file forever that no replica will
    /// ever receive: nothing reconciles the journal against the file tables at startup, and `fsck`
    /// does not audit it. Being one transaction it is also one fsync rather than four.
    ///
    /// Returns the journal serial the publication was recorded under.
    ///
    /// # Errors
    /// Returns a store error if the write, encode, or commit fails.
    pub fn publish_file(&self, file: &PublishedFile) -> Result<u64, MetaError> {
        let txn = self.db.begin_write()?;
        let serial = {
            if let Some(sibling) = &file.metadata {
                let value = metadata_value(sibling.url, sibling.metadata_sha256, sibling.source);
                let mut table = txn.open_table(METADATA)?;
                table.insert(sibling.artifact_sha256, value.as_str())?;
            }
            {
                let mut table = txn.open_table(UPLOAD)?;
                let key = format!("{}/{}/{}", file.index, file.normalized, file.filename);
                table.insert(key.as_str(), file.record)?;
            }
            {
                let mut table = txn.open_table(PROJECTS)?;
                let key = format!("{}/{}", file.index, file.normalized);
                table.insert(key.as_str(), file.display)?;
            }
            append_in_txn(
                &txn,
                "add-file",
                file.normalized,
                Some(file.version),
                Some(file.filename),
            )?
        };
        txn.commit()?;
        Ok(serial)
    }

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

    /// Promote a release onto `index`: its file records, its project, and its journal entry, together.
    ///
    /// One transaction, for the same reason [`MetaStore::publish_file`] is: a promotion the journal
    /// never records is invisible to every replica, and nothing reconciles that later.
    ///
    /// Returns the journal serial the promotion was recorded under.
    ///
    /// # Errors
    /// Returns a store error if the write, encode, or commit fails.
    pub fn promote_files(
        &self,
        index: &str,
        normalized: &str,
        display: &str,
        records: &[(String, Vec<u8>)],
    ) -> Result<u64, MetaError> {
        let txn = self.db.begin_write()?;
        let serial = {
            {
                let mut table = txn.open_table(UPLOAD)?;
                for (filename, record) in records {
                    let key = format!("{index}/{normalized}/{filename}");
                    table.insert(key.as_str(), record.as_slice())?;
                }
            }
            {
                let mut table = txn.open_table(PROJECTS)?;
                let key = format!("{index}/{normalized}");
                table.insert(key.as_str(), display)?;
            }
            append_in_txn(&txn, "promote", normalized, None, None)?
        };
        txn.commit()?;
        Ok(serial)
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
