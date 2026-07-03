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
const OVERRIDE: TableDefinition<&str, &str> = TableDefinition::new("overrides");
const SERIAL_KEY: &str = "serial";

/// A cached upstream simple-index response plus the metadata needed to revalidate it. The body is
/// the raw upstream document; velodex transforms it per request, so one cached page serves any route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedIndex {
    pub etag: Option<String>,
    pub last_serial: Option<u64>,
    pub fetched_at_unix: i64,
    #[serde(default)]
    pub content_type: Option<String>,
    /// The freshness lifetime upstream granted via `Cache-Control`; `None` means the server sent
    /// no usable lifetime and the configured fallback applies.
    #[serde(default)]
    pub fresh_secs: Option<i64>,
    pub body: Vec<u8>,
}

/// Marks the framed record encoding: a JSON header line, then the raw body bytes.
const RECORD_PREFIX: &[u8] = b"velodex1\n";

/// The revalidation metadata of a [`CachedIndex`], stored as one compact JSON line ahead of the
/// body. Serializing the body inside JSON would turn megabytes of page into an array of numbers,
/// quadrupling storage and dominating every warm read.
#[derive(Serialize, Deserialize)]
struct RecordHeader {
    etag: Option<String>,
    last_serial: Option<u64>,
    fetched_at_unix: i64,
    content_type: Option<String>,
    #[serde(default)]
    fresh_secs: Option<i64>,
}

impl CachedIndex {
    /// Encode to bytes for storage: prefix, header line, raw body.
    ///
    /// # Panics
    /// Never in practice: every header field is serializable.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let header = serde_json::to_vec(&RecordHeader {
            etag: self.etag.clone(),
            last_serial: self.last_serial,
            fetched_at_unix: self.fetched_at_unix,
            content_type: self.content_type.clone(),
            fresh_secs: self.fresh_secs,
        })
        .expect("record header always serializes");
        let mut out = Vec::with_capacity(RECORD_PREFIX.len() + header.len() + 1 + self.body.len());
        out.extend_from_slice(RECORD_PREFIX);
        out.extend_from_slice(&header);
        out.push(b'\n');
        out.extend_from_slice(&self.body);
        out
    }

    /// Decode from stored bytes, accepting both the framed encoding and the plain-JSON records
    /// written by earlier versions.
    ///
    /// # Errors
    /// Returns the serde error when `bytes` is not a valid encoding.
    pub fn decode(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        let Some((header, body)) = Self::split_framed(bytes) else {
            return serde_json::from_slice(bytes);
        };
        let header: RecordHeader = serde_json::from_slice(header)?;
        Ok(Self {
            etag: header.etag,
            last_serial: header.last_serial,
            fetched_at_unix: header.fetched_at_unix,
            content_type: header.content_type,
            fresh_secs: header.fresh_secs,
            body: body.to_vec(),
        })
    }

    /// Decode only the revalidation metadata, skipping the body copy; the refresher scans every
    /// record and needs nothing else.
    ///
    /// # Errors
    /// Returns the serde error when `bytes` is not a valid encoding.
    fn decode_freshness(bytes: &[u8]) -> Result<(i64, Option<i64>), serde_json::Error> {
        if let Some((header, _)) = Self::split_framed(bytes) {
            let header: RecordHeader = serde_json::from_slice(header)?;
            return Ok((header.fetched_at_unix, header.fresh_secs));
        }
        let record: Self = serde_json::from_slice(bytes)?;
        Ok((record.fetched_at_unix, record.fresh_secs))
    }

    /// Split a framed record into its header line and body, or `None` for legacy records.
    fn split_framed(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
        let rest = bytes.strip_prefix(RECORD_PREFIX)?;
        let split = rest.iter().position(|&byte| byte == b'\n')?;
        Some((&rest[..split], &rest[split + 1..]))
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
            txn.open_table(OVERRIDE)?;
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

    /// Store everything a freshly fetched mirror page produces in one transaction: the cached page
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
    pub fn put_mirror_page(
        &self,
        key: &str,
        record: &CachedIndex,
        index: &str,
        normalized: &str,
        display: &str,
        source: &str,
        files: &[(String, String)],
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
            let mut table = txn.open_table(FILE)?;
            for (sha256, url) in files {
                let value = format!(
                    "{url}
{source}"
                );
                table.insert(sha256.as_str(), value.as_str())?;
            }
            let mut table = txn.open_table(METADATA)?;
            for (wheel_sha256, url, metadata_sha256) in metadata {
                let value = format!(
                    "{url}
{metadata_sha256}
{source}"
                );
                table.insert(wheel_sha256.as_str(), value.as_str())?;
            }
        }
        txn.commit()?;
        Ok(())
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

    /// Every cached page's key, fetch timestamp, and upstream freshness lifetime, for the
    /// background refresher to find stale entries without loading the (potentially multi-megabyte)
    /// bodies into a list.
    ///
    /// # Errors
    /// Returns a store error if the read fails or a stored record cannot be decoded.
    pub fn list_index_pages(&self) -> Result<Vec<(String, i64, Option<i64>)>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(INDEX)?;
        table
            .iter()?
            .map(|entry| {
                let (key, value) = entry?;
                let (fetched_at, fresh_secs) = CachedIndex::decode_freshness(value.value())?;
                Ok((key.value().to_owned(), fetched_at, fresh_secs))
            })
            .collect()
    }

    /// Record the upstream URL a blob digest can be fetched from, and the name of the mirror it came
    /// from (so a fetch on a cache miss reuses that mirror's authentication).
    ///
    /// # Errors
    /// Returns a store error if the write or commit fails.
    pub fn put_file_url(&self, sha256: &str, url: &str, source: &str) -> Result<(), MetaError> {
        let value = format!("{url}\n{source}");
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(FILE)?;
            table.insert(sha256, value.as_str())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Look up the `(upstream url, mirror name)` for a blob digest.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn get_file_url(&self, sha256: &str) -> Result<Option<(String, String)>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(FILE)?;
        Ok(table.get(sha256)?.and_then(|value| split_pair(value.value())))
    }

    /// Record the PEP 658 metadata sibling for a wheel: keyed by the wheel's digest, storing the
    /// upstream `.metadata` URL and the metadata's own sha256 (for verify-on-fetch).
    ///
    /// # Errors
    /// Returns a store error if the write or commit fails.
    pub fn put_metadata(
        &self,
        wheel_sha256: &str,
        url: &str,
        metadata_sha256: &str,
        source: &str,
    ) -> Result<(), MetaError> {
        let value = format!("{url}\n{metadata_sha256}\n{source}");
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(METADATA)?;
            table.insert(wheel_sha256, value.as_str())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Look up a wheel's metadata sibling: `(upstream url, metadata sha256, mirror name)`.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn get_metadata(&self, wheel_sha256: &str) -> Result<Option<(String, String, String)>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(METADATA)?;
        Ok(table.get(wheel_sha256)?.and_then(|value| {
            let mut parts = value.value().splitn(3, '\n');
            Some((
                parts.next()?.to_owned(),
                parts.next()?.to_owned(),
                parts.next()?.to_owned(),
            ))
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

/// Split a `"first\nsecond"` stored value into its two owned halves.
fn split_pair(value: &str) -> Option<(String, String)> {
    value
        .split_once('\n')
        .map(|(first, second)| (first.to_owned(), second.to_owned()))
}
