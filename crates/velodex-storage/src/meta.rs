//! The metadata store: a redb database holding the monotonic serial counter and the cached
//! upstream simple-index records.
//!
//! redb is a pure-Rust, crash-safe, copy-on-write B-tree with one writer and many readers, so the
//! serial counter and cache records get snapshot-isolated reads without a global lock.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::path::Path;

use redb::{Database, ReadableDatabase as _, ReadableTable as _, TableDefinition};
use serde::{Deserialize, Serialize};

const SERIAL: TableDefinition<&str, u64> = TableDefinition::new("serial");
const INDEX: TableDefinition<&str, &[u8]> = TableDefinition::new("simple_index");
const FILE: TableDefinition<&str, &str> = TableDefinition::new("file_url");
const METADATA: TableDefinition<&str, &str> = TableDefinition::new("metadata");
const PROJECTS: TableDefinition<&str, &str> = TableDefinition::new("projects");
const PROJECT_STATUS: TableDefinition<&str, &[u8]> = TableDefinition::new("project_status");
const UPLOAD: TableDefinition<&str, &[u8]> = TableDefinition::new("uploads");
const OVERRIDE: TableDefinition<&str, &str> = TableDefinition::new("overrides");
const WEBHOOK_DELIVERY: TableDefinition<&str, &[u8]> = TableDefinition::new("webhook_delivery");
const WEBHOOK_DUE: TableDefinition<&str, &str> = TableDefinition::new("webhook_due");
const SERIAL_KEY: &str = "serial";
const WEBHOOK_SERIAL_KEY: &str = "webhook_delivery";

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

/// A cached simple-index record summary that does not copy the page body for framed records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedIndexSummary {
    pub fetched_at_unix: i64,
    pub fresh_secs: Option<i64>,
    pub body_bytes: u64,
    pub record_bytes: u64,
    pub last_serial: Option<u64>,
    pub content_type: Option<String>,
}

/// A cached simple-index record keyed by its metadata table key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedIndexPage {
    pub key: String,
    pub summary: CachedIndexSummary,
}

/// One project's explicit Simple API status marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectStatusRecord {
    pub status: Option<String>,
    pub reason: Option<String>,
}

/// Counts of metadata rows a project-cache purge plans or deletes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProjectCachePurgeCounts {
    pub index_pages: usize,
    pub project_records: usize,
    pub project_status_records: usize,
    pub file_url_records: usize,
    pub metadata_records: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebhookDeliveryStatus {
    Pending,
    Delivered,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookDeliveryRecord {
    pub id: String,
    pub index: String,
    pub target: String,
    pub event: String,
    pub payload: String,
    pub status: WebhookDeliveryStatus,
    pub attempts: u16,
    pub created_at_unix: i64,
    pub updated_at_unix: i64,
    pub next_attempt_at_unix: Option<i64>,
    pub response_status: Option<u16>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct NewWebhookDelivery<'a> {
    pub index: &'a str,
    pub target: &'a str,
    pub event: &'a str,
    pub payload: &'a str,
    pub created_at_unix: i64,
}

#[derive(Debug, Clone, Copy)]
pub struct WebhookDeliveryAttempt<'a> {
    pub status: WebhookDeliveryStatus,
    pub updated_at_unix: i64,
    pub next_attempt_at_unix: Option<i64>,
    pub response_status: Option<u16>,
    pub last_error: Option<&'a str>,
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
        let summary = Self::summary(bytes)?;
        Ok((summary.fetched_at_unix, summary.fresh_secs))
    }

    /// Decode cache-inspection metadata, skipping the body copy for framed records.
    ///
    /// # Errors
    /// Returns the serde error when `bytes` is not a valid encoding.
    pub fn summary(bytes: &[u8]) -> Result<CachedIndexSummary, serde_json::Error> {
        if let Some((header, body)) = Self::split_framed(bytes) {
            let header: RecordHeader = serde_json::from_slice(header)?;
            return Ok(CachedIndexSummary {
                fetched_at_unix: header.fetched_at_unix,
                fresh_secs: header.fresh_secs,
                body_bytes: body.len() as u64,
                record_bytes: bytes.len() as u64,
                last_serial: header.last_serial,
                content_type: header.content_type,
            });
        }
        let record: Self = serde_json::from_slice(bytes)?;
        Ok(CachedIndexSummary {
            fetched_at_unix: record.fetched_at_unix,
            fresh_secs: record.fresh_secs,
            body_bytes: record.body.len() as u64,
            record_bytes: bytes.len() as u64,
            last_serial: record.last_serial,
            content_type: record.content_type,
        })
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

/// A metadata scan error: either the store failed or the visitor rejected one row.
#[derive(Debug)]
pub enum MetaScanError<E> {
    Store(MetaError),
    Visit(E),
}

impl<E> From<MetaError> for MetaScanError<E> {
    fn from(err: MetaError) -> Self {
        Self::Store(err)
    }
}

impl<E: fmt::Display> fmt::Display for MetaScanError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(err) => err.fmt(formatter),
            Self::Visit(err) => err.fmt(formatter),
        }
    }
}

impl<E: Error + 'static> Error for MetaScanError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Store(err) => Some(err),
            Self::Visit(err) => Some(err),
        }
    }
}

/// The metadata store.
#[derive(Debug)]
pub struct MetaStore {
    db: Database,
}

/// Per-index package and upload counts for read-only status pages.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexSummary {
    pub project_count: u64,
    pub upload_count: u64,
    pub recent_uploads: Vec<RecentUpload>,
}

/// One uploaded file summary with token-free metadata only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecentUpload {
    pub project: String,
    pub filename: String,
    pub version: String,
    pub uploaded_at: Option<String>,
    pub size: Option<u64>,
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
            txn.open_table(PROJECT_STATUS)?;
            txn.open_table(UPLOAD)?;
            txn.open_table(OVERRIDE)?;
            txn.open_table(WEBHOOK_DELIVERY)?;
            txn.open_table(WEBHOOK_DUE)?;
        }
        txn.commit()?;
        Ok(Self { db })
    }

    /// Open an existing database without creating files or tables.
    ///
    /// # Errors
    /// Returns a store error if the database cannot be opened.
    pub fn open_existing(path: impl AsRef<Path>) -> Result<Self, MetaError> {
        Ok(Self {
            db: Database::open(path)?,
        })
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

    /// Insert a pending webhook delivery and return its delivery ID.
    ///
    /// # Errors
    /// Returns a store error if the write fails or the payload cannot be encoded.
    pub fn enqueue_webhook_delivery(&self, delivery: NewWebhookDelivery<'_>) -> Result<String, MetaError> {
        let txn = self.db.begin_write()?;
        let id = {
            let mut serials = txn.open_table(SERIAL)?;
            let next = serials.get(WEBHOOK_SERIAL_KEY)?.map_or(0, |value| value.value()) + 1;
            serials.insert(WEBHOOK_SERIAL_KEY, next)?;
            format!("wd_{next:016x}")
        };
        let record = WebhookDeliveryRecord {
            id: id.clone(),
            index: delivery.index.to_owned(),
            target: delivery.target.to_owned(),
            event: delivery.event.to_owned(),
            payload: delivery.payload.to_owned(),
            status: WebhookDeliveryStatus::Pending,
            attempts: 0,
            created_at_unix: delivery.created_at_unix,
            updated_at_unix: delivery.created_at_unix,
            next_attempt_at_unix: Some(delivery.created_at_unix),
            response_status: None,
            last_error: None,
        };
        {
            let bytes = serde_json::to_vec(&record)?;
            txn.open_table(WEBHOOK_DELIVERY)?
                .insert(id.as_str(), bytes.as_slice())?;
            txn.open_table(WEBHOOK_DUE)?
                .insert(due_key(delivery.created_at_unix, &id).as_str(), id.as_str())?;
        }
        txn.commit()?;
        Ok(id)
    }

    /// Pending webhook deliveries due at or before `now_unix`, ordered by due time.
    ///
    /// # Errors
    /// Returns a store error if the read fails or a record cannot be decoded.
    pub fn list_due_webhook_deliveries(
        &self,
        now_unix: i64,
        limit: usize,
    ) -> Result<Vec<WebhookDeliveryRecord>, MetaError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let txn = self.db.begin_read()?;
        let due = txn.open_table(WEBHOOK_DUE)?;
        let deliveries = txn.open_table(WEBHOOK_DELIVERY)?;
        let mut records = Vec::new();
        for entry in due.iter()? {
            let (key, id) = entry?;
            let Some(due_at) = due_key_time(key.value()) else {
                continue;
            };
            if due_at > now_unix {
                break;
            }
            let Some(record) = deliveries.get(id.value())? else {
                continue;
            };
            records.push(serde_json::from_slice(record.value())?);
            if records.len() == limit {
                break;
            }
        }
        Ok(records)
    }

    /// The next pending webhook retry timestamp.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn next_webhook_delivery_at(&self) -> Result<Option<i64>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(WEBHOOK_DUE)?;
        let mut entries = table.iter()?;
        Ok(match entries.next().transpose()? {
            Some((key, _)) => due_key_time(key.value()),
            None => None,
        })
    }

    /// Apply one delivery attempt result, returning the updated record when it still exists.
    ///
    /// # Errors
    /// Returns a store error if the write fails or the record cannot be decoded or encoded.
    pub fn update_webhook_delivery(
        &self,
        id: &str,
        attempt: WebhookDeliveryAttempt<'_>,
    ) -> Result<Option<WebhookDeliveryRecord>, MetaError> {
        let txn = self.db.begin_write()?;
        let Some(mut record) = ({
            let table = txn.open_table(WEBHOOK_DELIVERY)?;
            table
                .get(id)?
                .map(|value| serde_json::from_slice::<WebhookDeliveryRecord>(value.value()))
                .transpose()?
        }) else {
            return Ok(None);
        };
        if let Some(next) = record.next_attempt_at_unix {
            let key = due_key(next, &record.id);
            txn.open_table(WEBHOOK_DUE)?.remove(key.as_str())?;
        }
        record.status = attempt.status;
        record.attempts += 1;
        record.updated_at_unix = attempt.updated_at_unix;
        record.next_attempt_at_unix = attempt.next_attempt_at_unix;
        record.response_status = attempt.response_status;
        record.last_error = attempt.last_error.map(str::to_owned);
        {
            let bytes = serde_json::to_vec(&record)?;
            txn.open_table(WEBHOOK_DELIVERY)?.insert(id, bytes.as_slice())?;
            if record.status == WebhookDeliveryStatus::Pending
                && let Some(next) = record.next_attempt_at_unix
            {
                txn.open_table(WEBHOOK_DUE)?.insert(due_key(next, id).as_str(), id)?;
            }
        }
        txn.commit()?;
        Ok(Some(record))
    }

    /// Fetch one webhook delivery by ID.
    ///
    /// # Errors
    /// Returns a store error if the read fails or the record cannot be decoded.
    pub fn get_webhook_delivery(&self, id: &str) -> Result<Option<WebhookDeliveryRecord>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(WEBHOOK_DELIVERY)?;
        Ok(table
            .get(id)?
            .map(|value| serde_json::from_slice(value.value()))
            .transpose()?)
    }

    /// List webhook delivery records by delivery ID.
    ///
    /// # Errors
    /// Returns a store error if the read fails or a record cannot be decoded.
    pub fn list_webhook_deliveries(&self) -> Result<Vec<WebhookDeliveryRecord>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = txn.open_table(WEBHOOK_DELIVERY)?;
        let mut deliveries = Vec::new();
        for entry in table.iter()? {
            let (_, value) = entry?;
            deliveries.push(serde_json::from_slice(value.value())?);
        }
        Ok(deliveries)
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
        project_status: Option<&str>,
        project_status_reason: Option<&str>,
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

    /// Visit raw project-display records.
    ///
    /// # Errors
    /// Returns a scan error if the store read fails or the visitor returns an error.
    pub fn scan_project_records<E>(
        &self,
        mut visit: impl FnMut(&str, &str) -> Result<(), E>,
    ) -> Result<(), MetaScanError<E>> {
        let txn = self.db.begin_read().map_err(MetaError::from)?;
        let table = txn.open_table(PROJECTS).map_err(MetaError::from)?;
        for entry in table.iter().map_err(MetaError::from)? {
            let (key, value) = entry.map_err(MetaError::from)?;
            visit(key.value(), value.value()).map_err(MetaScanError::Visit)?;
        }
        Ok(())
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
        let value = format!("{url}\n{metadata_sha256}\n{source}");
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(METADATA)?;
            table.insert(artifact_sha256, value.as_str())?;
        }
        txn.commit()?;
        Ok(())
    }

    /// Look up an artifact's metadata sibling: `(upstream url, metadata sha256, mirror name)`.
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

    /// Fetch a project's display name on one index.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn get_project(&self, index: &str, normalized: &str) -> Result<Option<String>, MetaError> {
        let key = format!("{index}/{normalized}");
        let txn = self.db.begin_read()?;
        let table = txn.open_table(PROJECTS)?;
        Ok(table.get(key.as_str())?.map(|value| value.value().to_owned()))
    }

    /// Count the rows a project-cache purge would remove.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn count_project_cache_purge(
        &self,
        index: &str,
        normalized: &str,
        file_digests: &[String],
        metadata_digests: &[String],
    ) -> Result<ProjectCachePurgeCounts, MetaError> {
        let key = format!("{index}/{normalized}");
        let txn = self.db.begin_read()?;
        let index_pages = usize::from(txn.open_table(INDEX)?.get(key.as_str())?.is_some());
        let project_records = usize::from(txn.open_table(PROJECTS)?.get(key.as_str())?.is_some());
        let project_status_records = usize::from(txn.open_table(PROJECT_STATUS)?.get(key.as_str())?.is_some());
        let file_table = txn.open_table(FILE)?;
        let mut file_url_records = 0;
        for digest in file_digests {
            file_url_records += usize::from(file_table.get(digest.as_str())?.is_some());
        }
        let metadata_table = txn.open_table(METADATA)?;
        let mut metadata_records = 0;
        for digest in metadata_digests {
            metadata_records += usize::from(metadata_table.get(digest.as_str())?.is_some());
        }
        Ok(ProjectCachePurgeCounts {
            index_pages,
            project_records,
            project_status_records,
            file_url_records,
            metadata_records,
        })
    }

    /// Delete cached metadata rows for one project.
    ///
    /// # Errors
    /// Returns a store error if the write or commit fails.
    pub fn delete_project_cache(
        &self,
        index: &str,
        normalized: &str,
        file_digests: &[String],
        metadata_digests: &[String],
    ) -> Result<ProjectCachePurgeCounts, MetaError> {
        let key = format!("{index}/{normalized}");
        let txn = self.db.begin_write()?;
        let counts = {
            let index_pages = {
                let mut table = txn.open_table(INDEX)?;
                usize::from(table.remove(key.as_str())?.is_some())
            };
            let project_records = {
                let mut table = txn.open_table(PROJECTS)?;
                usize::from(table.remove(key.as_str())?.is_some())
            };
            let project_status_records = {
                let mut table = txn.open_table(PROJECT_STATUS)?;
                usize::from(table.remove(key.as_str())?.is_some())
            };
            let file_url_records = {
                let mut table = txn.open_table(FILE)?;
                let mut removed = 0;
                for digest in file_digests {
                    removed += usize::from(table.remove(digest.as_str())?.is_some());
                }
                removed
            };
            let metadata_records = {
                let mut table = txn.open_table(METADATA)?;
                let mut removed = 0;
                for digest in metadata_digests {
                    removed += usize::from(table.remove(digest.as_str())?.is_some());
                }
                removed
            };
            ProjectCachePurgeCounts {
                index_pages,
                project_records,
                project_status_records,
                file_url_records,
                metadata_records,
            }
        };
        txn.commit()?;
        Ok(counts)
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

    /// Summarize observed projects and uploads for configured indexes in one read transaction.
    ///
    /// # Errors
    /// Returns a store error if the read fails.
    pub fn summarize_indexes(
        &self,
        index_names: &[String],
        recent_limit: usize,
    ) -> Result<HashMap<String, IndexSummary>, MetaError> {
        let mut summaries: HashMap<String, IndexSummary> = index_names
            .iter()
            .map(|name| (name.clone(), IndexSummary::default()))
            .collect();
        let txn = self.db.begin_read()?;
        let projects = txn.open_table(PROJECTS)?;
        let ordered = ordered_index_names(index_names);
        for entry in projects.iter()? {
            let (key, _) = entry?;
            if let Some(index) = matching_index(key.value(), &ordered)
                && let Some(summary) = summaries.get_mut(index)
            {
                summary.project_count += 1;
            }
        }
        let uploads = txn.open_table(UPLOAD)?;
        for entry in uploads.iter()? {
            let (key, value) = entry?;
            if let Some((index, project, fallback_filename)) = upload_key_parts(key.value(), &ordered)
                && let Some(summary) = summaries.get_mut(index)
            {
                summary.upload_count += 1;
                if let Some(upload) = recent_upload(project, fallback_filename, value.value()) {
                    push_recent(&mut summary.recent_uploads, upload, recent_limit);
                }
            }
        }
        Ok(summaries)
    }
}

fn due_key(timestamp: i64, id: &str) -> String {
    let sortable = u64::from_be_bytes(timestamp.to_be_bytes()) ^ (1_u64 << 63);
    format!("{sortable:020}/{id}")
}

fn due_key_time(key: &str) -> Option<i64> {
    let raw = key.split_once('/')?.0.parse::<u64>().ok()?;
    Some(i64::from_be_bytes((raw ^ (1_u64 << 63)).to_be_bytes()))
}

/// Split a `"first\nsecond"` stored value into its two owned halves.
fn split_pair(value: &str) -> Option<(String, String)> {
    value
        .split_once('\n')
        .map(|(first, second)| (first.to_owned(), second.to_owned()))
}

fn ordered_index_names(index_names: &[String]) -> Vec<&str> {
    let mut ordered: Vec<&str> = index_names.iter().map(String::as_str).collect();
    ordered.sort_by_key(|name| std::cmp::Reverse(name.len()));
    ordered
}

fn matching_index<'a>(key: &str, ordered: &'a [&str]) -> Option<&'a str> {
    ordered
        .iter()
        .copied()
        .find(|index| key.strip_prefix(index).is_some_and(|rest| rest.starts_with('/')))
}

fn upload_key_parts<'a>(key: &'a str, ordered: &'a [&str]) -> Option<(&'a str, &'a str, &'a str)> {
    let index = matching_index(key, ordered)?;
    let rest = key.strip_prefix(index)?.strip_prefix('/')?;
    let (project, filename) = rest.split_once('/')?;
    Some((index, project, filename))
}

fn recent_upload(project: &str, fallback_filename: &str, bytes: &[u8]) -> Option<RecentUpload> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    Some(RecentUpload {
        project: project.to_owned(),
        filename: value["file"]["filename"]
            .as_str()
            .unwrap_or(fallback_filename)
            .to_owned(),
        version: value["version"].as_str().unwrap_or_default().to_owned(),
        uploaded_at: value["file"]["upload-time"].as_str().map(str::to_owned),
        size: value["file"]["size"].as_u64(),
    })
}

fn push_recent(recent: &mut Vec<RecentUpload>, upload: RecentUpload, limit: usize) {
    if limit == 0 {
        return;
    }
    recent.push(upload);
    recent.sort_by(|left, right| {
        right
            .uploaded_at
            .cmp(&left.uploaded_at)
            .then_with(|| left.filename.cmp(&right.filename))
    });
    recent.truncate(limit);
}
