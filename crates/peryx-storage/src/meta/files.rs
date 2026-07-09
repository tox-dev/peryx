use std::collections::HashMap;

use redb::{ReadableDatabase as _, ReadableTable as _};

use super::error::{MetaError, MetaScanError};
use super::{FILE, METADATA, MetaStore, file_source_value, metadata_value};

/// The upstream source for a cached artifact digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSource {
    pub url: String,
    pub source: String,
    pub size: Option<u64>,
}

impl MetaStore {
    /// Record the upstream URL a blob digest can be fetched from, and the name of the cached index it came
    /// from (so a fetch on a cache miss reuses that index's authentication).
    ///
    /// # Errors
    /// Returns a store error if the write or commit fails.
    pub fn put_file_url(&self, sha256: &str, url: &str, source: &str) -> Result<(), MetaError> {
        let value = file_source_value(url, source, None);
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(FILE)?;
            table.insert(sha256, value.as_str())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Look up the `(upstream url, index name)` for a blob digest.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn get_file_url(&self, sha256: &str) -> Result<Option<FileSource>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(FILE)?;
        Ok(table.get(sha256)?.and_then(|value| split_file_source(value.value())))
    }

    /// Visit raw file URL records, keyed by artifact digest.
    ///
    /// # Errors
    /// Returns a scan error if the store read fails or the visitor returns an error.
    pub fn scan_file_urls<E>(
        &self,
        mut visit: impl FnMut(&str, &str) -> Result<(), E>,
    ) -> Result<(), MetaScanError<E>> {
        let txn = self.db.begin_read().map_err(MetaError::from)?;
        let table = txn.open_table(FILE).map_err(MetaError::from)?;
        for entry in table.iter().map_err(MetaError::from)? {
            let (key, value) = entry.map_err(MetaError::from)?;
            visit(key.value(), value.value()).map_err(MetaScanError::Visit)?;
        }
        Ok(())
    }

    /// Record the PEP 658 metadata sibling for an artifact: keyed by the artifact's digest,
    /// storing the upstream `.metadata` URL and the metadata's own sha256 (for verify-on-fetch).
    ///
    /// # Errors
    /// Returns a store error if the write or commit fails.
    pub fn put_metadata(
        &self,
        artifact_sha256: &str,
        url: &str,
        metadata_sha256: &str,
        source: &str,
    ) -> Result<(), MetaError> {
        let value = metadata_value(url, metadata_sha256, source);
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(METADATA)?;
            table.insert(artifact_sha256, value.as_str())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Look up an artifact's metadata sibling: `(upstream url, metadata sha256, index name)`.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn get_metadata(&self, artifact_sha256: &str) -> Result<Option<(String, String, String)>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(METADATA)?;
        Ok(table.get(artifact_sha256)?.and_then(|value| {
            let mut parts = value.value().splitn(3, '\n');
            Some((
                parts.next()?.to_owned(),
                parts.next()?.to_owned(),
                parts.next()?.to_owned(),
            ))
        }))
    }

    /// Look up metadata sha256 values for many artifact digests in one read transaction.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn get_metadata_digests<'a>(
        &self,
        artifact_sha256s: impl IntoIterator<Item = &'a str>,
    ) -> Result<HashMap<String, String>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(METADATA)?;
        let mut metadata = HashMap::new();
        for artifact_sha256 in artifact_sha256s {
            let Some(value) = table.get(artifact_sha256)? else {
                continue;
            };
            let mut parts = value.value().splitn(3, '\n');
            let (_url, Some(metadata_sha256), _source) = (parts.next(), parts.next(), parts.next()) else {
                continue;
            };
            metadata.insert(artifact_sha256.to_owned(), metadata_sha256.to_owned());
        }
        Ok(metadata)
    }

    /// Visit raw PEP 658 metadata records, keyed by wheel digest.
    ///
    /// # Errors
    /// Returns a scan error if the store read fails or the visitor returns an error.
    pub fn scan_metadata_records<E>(
        &self,
        mut visit: impl FnMut(&str, &str) -> Result<(), E>,
    ) -> Result<(), MetaScanError<E>> {
        let txn = self.db.begin_read().map_err(MetaError::from)?;
        let table = txn.open_table(METADATA).map_err(MetaError::from)?;
        for entry in table.iter().map_err(MetaError::from)? {
            let (key, value) = entry.map_err(MetaError::from)?;
            visit(key.value(), value.value()).map_err(MetaScanError::Visit)?;
        }
        Ok(())
    }
}

fn split_file_source(value: &str) -> Option<FileSource> {
    let mut parts = value.splitn(3, '\n');
    Some(FileSource {
        url: parts.next()?.to_owned(),
        source: parts.next()?.to_owned(),
        size: parts.next().and_then(|size| size.parse().ok()),
    })
}
