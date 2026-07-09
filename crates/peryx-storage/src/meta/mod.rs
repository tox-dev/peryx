//! The metadata store: a redb database holding the monotonic serial counter and the cached
//! upstream simple-index records.
//!
//! redb is a pure-Rust, crash-safe, copy-on-write B-tree with one writer and many readers, so the
//! serial counter and cache records get snapshot-isolated reads without a global lock.

use std::path::Path;

use redb::{Database, TableDefinition};

mod error;
mod files;
mod index;
mod journal;
mod projects;
mod record;
mod summary;
mod uploads;
mod webhook;

pub use error::{MetaError, MetaScanError};
pub use files::FileSource;
pub use journal::JournalEntry;
pub use projects::ProjectCachePurgeCounts;
pub use record::{CachedIndex, CachedIndexPage, CachedIndexSummary, ProjectStatusRecord};
pub use summary::{IndexSummary, RecentUpload};
pub use webhook::{NewWebhookDelivery, WebhookDeliveryAttempt, WebhookDeliveryRecord, WebhookDeliveryStatus};

const SERIAL: TableDefinition<&str, u64> = TableDefinition::new("serial");
const INDEX: TableDefinition<&str, &[u8]> = TableDefinition::new("index_document");
const FILE: TableDefinition<&str, &str> = TableDefinition::new("artifact_source");
const METADATA: TableDefinition<&str, &str> = TableDefinition::new("metadata_sidecar");
const PROJECTS: TableDefinition<&str, &str> = TableDefinition::new("projects");
const PROJECT_STATUS: TableDefinition<&str, &[u8]> = TableDefinition::new("project_status");
const UPLOAD: TableDefinition<&str, &[u8]> = TableDefinition::new("uploads");
const OVERRIDE: TableDefinition<&str, &str> = TableDefinition::new("overrides");
const WEBHOOK_DELIVERY: TableDefinition<&str, &[u8]> = TableDefinition::new("webhook_delivery");
const WEBHOOK_DUE: TableDefinition<&str, &str> = TableDefinition::new("webhook_due");
const JOURNAL: TableDefinition<u64, &[u8]> = TableDefinition::new("journal");
/// A neutral byte key-value table an ecosystem driver owns end to end: the store never interprets a
/// key or value, so a format (OCI manifests and tags, say) serializes into its own namespace without
/// the store growing format-specific tables.
const DRIVER_KV: TableDefinition<&str, &[u8]> = TableDefinition::new("driver_kv");
const SERIAL_KEY: &str = "serial";
const WEBHOOK_SERIAL_KEY: &str = "webhook_delivery";

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
            txn.open_table(PROJECT_STATUS)?;
            txn.open_table(UPLOAD)?;
            txn.open_table(OVERRIDE)?;
            txn.open_table(WEBHOOK_DELIVERY)?;
            txn.open_table(WEBHOOK_DUE)?;
            txn.open_table(DRIVER_KV)?;
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
}

fn file_source_value(url: &str, source: &str, size: Option<u64>) -> String {
    size.map_or_else(|| format!("{url}\n{source}"), |size| format!("{url}\n{source}\n{size}"))
}

fn metadata_value(url: &str, metadata_sha256: &str, source: &str) -> String {
    format!("{url}\n{metadata_sha256}\n{source}")
}
