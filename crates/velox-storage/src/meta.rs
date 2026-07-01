//! The metadata store: a redb database holding the monotonic serial counter and the cached
//! upstream simple-index records.
//!
//! redb is a pure-Rust, crash-safe, copy-on-write B-tree with one writer and many readers, so the
//! serial counter and cache records get snapshot-isolated reads without a global lock.

use std::path::Path;

use redb::{Database, ReadableDatabase as _, ReadableTable as _, TableDefinition};
use serde::{Deserialize, Serialize};

const SERIAL: TableDefinition<&str, u64> = TableDefinition::new("serial");
const INDEX: TableDefinition<&str, &[u8]> = TableDefinition::new("simple_index");
const FILE: TableDefinition<&str, &str> = TableDefinition::new("file_url");
const METADATA: TableDefinition<&str, &str> = TableDefinition::new("metadata");
const PROJECTS: TableDefinition<&str, &str> = TableDefinition::new("projects");
const UPLOAD: TableDefinition<&str, &[u8]> = TableDefinition::new("uploads");
const SERIAL_KEY: &str = "serial";

/// A cached upstream simple-index response plus the metadata needed to revalidate it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedIndex {
    pub etag: Option<String>,
    pub last_serial: Option<u64>,
    pub fetched_at_unix: i64,
    pub body: Vec<u8>,
}

impl CachedIndex {
    /// Encode to bytes for storage.
    ///
    /// # Panics
    /// Never in practice: every field of `CachedIndex` is serializable.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("CachedIndex always serializes")
    }

    /// Decode from stored bytes.
    ///
    /// # Errors
    /// Returns the serde error when `bytes` is not a valid encoding.
    pub fn decode(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }
}

/// An error from the metadata store.
#[derive(Debug, thiserror::Error)]
pub enum MetaError {
    #[error(transparent)]
    Database(#[from] redb::DatabaseError),
    #[error(transparent)]
    Transaction(#[from] redb::TransactionError),
    #[error(transparent)]
    Table(#[from] redb::TableError),
    #[error(transparent)]
    Storage(#[from] redb::StorageError),
    #[error(transparent)]
    Commit(#[from] redb::CommitError),
    #[error(transparent)]
    Decode(#[from] serde_json::Error),
}

/// The metadata store.
#[derive(Debug)]
pub struct MetaStore {
    db: Database,
}

impl MetaStore {
    /// Open (creating if needed) the database at `path`, initializing its tables so later reads
    /// never race a missing table.
    ///
    /// # Errors
    /// Returns a store error if the database cannot be opened or initialized.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, MetaError> {
        let db = Database::create(path)?;
        let txn = db.begin_write()?;
        {
            txn.open_table(SERIAL)?;
            txn.open_table(INDEX)?;
            txn.open_table(FILE)?;
            txn.open_table(METADATA)?;
            txn.open_table(PROJECTS)?;
            txn.open_table(UPLOAD)?;
        }
        txn.commit()?;
        Ok(Self { db })
    }

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

    /// Record the upstream URL a blob digest can be fetched from.
    ///
    /// # Errors
    /// Returns a store error if the write or commit fails.
    pub fn put_file_url(&self, sha256: &str, url: &str) -> Result<(), MetaError> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(FILE)?;
            table.insert(sha256, url)?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Look up the upstream URL for a blob digest.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn get_file_url(&self, sha256: &str) -> Result<Option<String>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(FILE)?;
        Ok(table.get(sha256)?.map(|value| value.value().to_owned()))
    }

    /// Record the PEP 658 metadata sibling for a wheel: keyed by the wheel's digest, storing the
    /// upstream `.metadata` URL and the metadata's own sha256 (for verify-on-fetch).
    ///
    /// # Errors
    /// Returns a store error if the write or commit fails.
    pub fn put_metadata(&self, wheel_sha256: &str, url: &str, metadata_sha256: &str) -> Result<(), MetaError> {
        let value = format!("{url}\n{metadata_sha256}");
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(METADATA)?;
            table.insert(wheel_sha256, value.as_str())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Look up a wheel's metadata sibling: `(upstream url, metadata sha256)`.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn get_metadata(&self, wheel_sha256: &str) -> Result<Option<(String, String)>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(METADATA)?;
        Ok(table.get(wheel_sha256)?.and_then(|value| {
            value
                .value()
                .split_once('\n')
                .map(|(url, digest)| (url.to_owned(), digest.to_owned()))
        }))
    }

    /// Record that `display` (a project's display name) has been observed on `index`, keyed by its
    /// normalized name so re-observations do not duplicate.
    ///
    /// # Errors
    /// Returns a store error if the write or commit fails.
    pub fn put_project(&self, index: &str, normalized: &str, display: &str) -> Result<(), MetaError> {
        let key = format!("{index}/{normalized}");
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(PROJECTS)?;
            table.insert(key.as_str(), display)?;
        }
        txn.commit()?;
        Ok(())
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

    /// List the serialized file records uploaded for `normalized` on `index`, sorted by filename.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn list_uploads(&self, index: &str, normalized: &str) -> Result<Vec<Vec<u8>>, MetaError> {
        let prefix = format!("{index}/{normalized}/");
        let txn = self.db.begin_read()?;
        let table = txn.open_table(UPLOAD)?;
        let mut records = Vec::new();
        for entry in table.iter()? {
            let (key, value) = entry?;
            if key.value().starts_with(&prefix) {
                records.push(value.value().to_vec());
            }
        }
        Ok(records)
    }

    /// List the display names of projects observed on `index`, sorted.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn list_projects(&self, index: &str) -> Result<Vec<String>, MetaError> {
        let prefix = format!("{index}/");
        let txn = self.db.begin_read()?;
        let table = txn.open_table(PROJECTS)?;
        let mut names = Vec::new();
        for entry in table.iter()? {
            let (key, value) = entry?;
            if key.value().starts_with(&prefix) {
                names.push(value.value().to_owned());
            }
        }
        names.sort();
        Ok(names)
    }
}
