//! The metadata store: a redb database holding the monotonic serial counter and the cached
//! upstream simple-index records.
//!
//! redb is a pure-Rust, crash-safe, copy-on-write B-tree with one writer and many readers, so the
//! serial counter and cache records get snapshot-isolated reads without a global lock.

use std::path::Path;
use std::sync::Arc;

use redb::{Database, TableDefinition};

mod analytics;
mod error;
mod index;
mod job;
mod journal;
mod policy_decision;
mod quota;
mod user;
mod webhook;
mod writer;

pub use analytics::AnalyticsHandle;
pub use error::{MetaError, MetaScanError, WriterIdentityError};
pub use index::DriverTxn;
pub use job::{JobKind, JobOutcome, JobRunRecord, JobState, NewJobRun};
pub use journal::{DriverBlobReference, DriverMutation, JournalRecord, JournalSnapshot};
pub use policy_decision::{
    NewPolicyDecision, PolicyDecisionItem, PolicyDecisionPage, PolicyDecisionQuery, PolicyDecisionQueryError,
    PolicyDecisionRecord, PolicyDecisionStoreError, PolicyInputGeneration,
};
pub use quota::{
    AccountingClass, NewQuotaReservation, QuotaError, QuotaLimit, QuotaLimits, QuotaProjectUsage, QuotaRepairReport,
    QuotaReservationRecord, QuotaReservationState, QuotaUsage, QuotaValue,
};
pub use user::UserStoreError;
pub use webhook::{NewWebhookDelivery, WebhookDeliveryAttempt, WebhookDeliveryRecord, WebhookDeliveryStatus};

const SERIAL: TableDefinition<&str, u64> = TableDefinition::new("serial");
const WEBHOOK_DELIVERY: TableDefinition<&str, &[u8]> = TableDefinition::new("webhook_delivery");
const WEBHOOK_DUE: TableDefinition<&str, &str> = TableDefinition::new("webhook_due");
const JOB_RUN: TableDefinition<&str, &[u8]> = TableDefinition::new("job_run");
const POLICY_DECISION: TableDefinition<&str, &[u8]> = TableDefinition::new("policy_decision");
const POLICY_DECISION_CURRENT: TableDefinition<&str, &str> = TableDefinition::new("policy_decision_current");
const POLICY_DECISION_CURRENT_ID: TableDefinition<&str, &str> = TableDefinition::new("policy_decision_current_id");
const POLICY_INPUT_GENERATION: TableDefinition<&str, &[u8]> = TableDefinition::new("policy_input_generation");
const QUOTA_USAGE: TableDefinition<&str, &[u8]> = TableDefinition::new("quota_usage");
const QUOTA_PROJECT: TableDefinition<&str, &[u8]> = TableDefinition::new("quota_project");
const QUOTA_VERSION: TableDefinition<&str, &[u8]> = TableDefinition::new("quota_version");
const QUOTA_BLOB: TableDefinition<&str, &[u8]> = TableDefinition::new("quota_blob");
const QUOTA_RESERVATION: TableDefinition<&str, &[u8]> = TableDefinition::new("quota_reservation");
const QUOTA_PENDING: TableDefinition<u128, u8> = TableDefinition::new("quota_pending");
const JOURNAL: TableDefinition<u64, &[u8]> = TableDefinition::new("journal");
const WRITER: TableDefinition<&str, &str> = TableDefinition::new("writer");
const JOURNAL_MUTATIONS: TableDefinition<u64, &[u8]> = TableDefinition::new("journal_mutations");
const JOURNAL_BLOBS: TableDefinition<u64, &[u8]> = TableDefinition::new("journal_blobs");
/// A neutral byte key-value table an ecosystem driver owns end to end: the store never interprets a
/// key or value, so a format (OCI manifests and tags, say) serializes into its own namespace without
/// the store growing format-specific tables.
const DRIVER_KV: TableDefinition<&str, &[u8]> = TableDefinition::new("driver_kv");
/// The persisted download-usage aggregates, held as one opaque snapshot blob the metrics aggregator
/// owns; see [`AnalyticsHandle`].
const ANALYTICS: TableDefinition<&str, &[u8]> = TableDefinition::new("analytics");
const USER: TableDefinition<&str, &[u8]> = TableDefinition::new("server_user");
const USER_NAME: TableDefinition<&str, &str> = TableDefinition::new("server_user_name");
const USER_EVENT: TableDefinition<&str, &[u8]> = TableDefinition::new("server_user_event");
const SERIAL_KEY: &str = "serial";
const WEBHOOK_SERIAL_KEY: &str = "webhook_delivery";
const JOB_SERIAL_KEY: &str = "job_run";
const POLICY_DECISION_SERIAL_KEY: &str = "policy_decision";
const ANALYTICS_KEY: &str = "downloads";
const WRITER_KEY: &str = "active";

/// A set of driver-owned writes to apply in one transaction.
///
/// Applied through [`MetaStore::commit_driver_batch`]. Keys and values are opaque bytes the store
/// never interprets, so an ecosystem batches a multi-row mutation (a cached page, a publish)
/// atomically without the store growing a table per format.
#[derive(Debug, Default)]
pub struct DriverBatch {
    puts: Vec<(String, Vec<u8>)>,
    deletes: Vec<String>,
}

impl DriverBatch {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Upsert `key` to `value` when the batch commits.
    pub fn put(&mut self, key: String, value: Vec<u8>) {
        self.puts.push((key, value));
    }

    /// Remove `key` when the batch commits.
    pub fn delete(&mut self, key: String) {
        self.deletes.push(key);
    }
}

/// The metadata store.
#[derive(Debug, Clone)]
pub struct MetaStore {
    db: Arc<Database>,
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
            txn.open_table(WEBHOOK_DELIVERY)?;
            txn.open_table(WEBHOOK_DUE)?;
            txn.open_table(JOB_RUN)?;
            txn.open_table(POLICY_DECISION)?;
            txn.open_table(POLICY_DECISION_CURRENT)?;
            txn.open_table(POLICY_DECISION_CURRENT_ID)?;
            txn.open_table(POLICY_INPUT_GENERATION)?;
            txn.open_table(QUOTA_USAGE)?;
            txn.open_table(QUOTA_PROJECT)?;
            txn.open_table(QUOTA_VERSION)?;
            txn.open_table(QUOTA_BLOB)?;
            txn.open_table(QUOTA_RESERVATION)?;
            txn.open_table(QUOTA_PENDING)?;
            txn.open_table(JOURNAL)?;
            txn.open_table(WRITER)?;
            txn.open_table(JOURNAL_MUTATIONS)?;
            txn.open_table(JOURNAL_BLOBS)?;
            txn.open_table(DRIVER_KV)?;
            txn.open_table(ANALYTICS)?;
            txn.open_table(USER)?;
            txn.open_table(USER_NAME)?;
            txn.open_table(USER_EVENT)?;
        }
        txn.commit()?;
        Ok(Self { db: Arc::new(db) })
    }

    /// Open an existing database without creating files or tables.
    ///
    /// # Errors
    /// Returns a store error if the database cannot be opened.
    pub fn open_existing(path: impl AsRef<Path>) -> Result<Self, MetaError> {
        Ok(Self {
            db: Arc::new(Database::open(path)?),
        })
    }
}
