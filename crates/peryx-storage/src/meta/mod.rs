//! Redb provides crash-safe transactions and snapshot-isolated reads for repository metadata and its
//! monotonic serial.

use std::path::Path;
use std::sync::Arc;

use redb::{Database, ReadOnlyDatabase, ReadableDatabase as _, TableDefinition};

mod analytics;
mod blob_chunk_digest;
mod blob_placement;
mod bootstrap;
mod error;
mod external_identity;
#[cfg(test)]
#[path = "../../tests/unit/meta/fault.rs"]
mod fault;
mod finalize;
mod frontier;
mod index;
mod ingress_intent;
mod job;
mod job_lease;
mod journal;
mod migration;
mod operation_outcome;
mod placement;
mod policy_decision;
mod quota;
mod reclaim_guard;
mod reclamation;
mod reconcile;
mod repository;
mod revocation;
mod role_grant;
mod scoped_token;
mod transfer_audit;
mod user;
mod version;
mod visibility;
mod webhook;
mod writer;

pub use analytics::{AnalyticsCheckpoint, AnalyticsHandle};
pub use bootstrap::AdministratorBootstrapError;
pub use error::{MetaError, MetaScanError, WriterIdentityError};
pub use external_identity::ExternalIdentityStoreError;
pub use finalize::{FinalizeOutcome, FinalizedWrite};
pub use index::{DriverEntries, DriverReadTxn, DriverTxn};
pub use ingress_intent::{
    BackpressureState, IntentAdmission, IntentLimits, IntentPhase, IntentStageOutcome, IntentStageResult,
    IntentTransition, IntentUsage, StagedIntent,
};
pub use job::{
    FinishJobRun, JobKind, JobOutcome, JobRunPage, JobRunQuery, JobRunQueryError, JobRunRecord, JobRunStoreError,
    JobState, NewJobRun,
};
pub use job_lease::{ClaimOutcome, JobLease, JobLeaseError, LeaseState};
pub use journal::{
    DriverBlobReference, DriverCommit, DriverMutation, JournalCommit, JournalEntry, JournalRecord, JournalSnapshot,
};
pub use migration::{
    LegacyMetadataSource, MetadataMigration, MetadataMigrationError, MetadataMigrationReport, MetadataRecord,
    MetadataRecordSet, MetadataValueKind,
};
pub use operation_outcome::{
    OperationClaim, OperationOutcomeError, OperationOutcomeHealth, OperationOutcomePage, OperationOutcomeQuery,
    OperationOutcomeQueryError, OperationOutcomeRecord, OperationOutcomeRow, OperationResult, OperationState,
};
pub use peryx_core::ObservedFrontier;
pub use peryx_ha::{
    ArtifactOrigin, ArtifactPlacement, ArtifactPlacementHealth, ArtifactPlacementPage, ArtifactPlacementQuery,
    ArtifactPlacementRow, ArtifactSource, BackendId, BackendLocation, BlobPlacementDecisionError, BlobPlacementFailure,
    BlobPlacementGroupPage, BlobPlacementKey, BlobPlacementOutcome, BlobPlacementPage, BlobPlacementRecord,
    BlobPlacementRouting, BlobPlacementState, BlobPlacementStatus, BlobPlacementTransition, ByteAvailability,
    CompareWrite, DataCenterId, MAX_PLACEMENTS_PER_DIGEST, MAX_REPAIR_BATCH, NewReconcileEntry, PlacementEvent,
    PlacementKeyError, PlacementRepairPage, ReadyOutcome, ReclamationDecisionError, ReclamationProgress,
    ReclamationSnapshot, ReclamationState, ReclamationStatus, ReclamationTombstone, ReconcileEnqueue, ReconcileEntry,
    ReconcilePage, SelectOutcome, SkipReason, TransferAudit,
};
pub use placement::ArtifactPlacementQueryError;
pub use policy_decision::{
    NewPolicyDecision, PolicyDecisionItem, PolicyDecisionPage, PolicyDecisionQuery, PolicyDecisionQueryError,
    PolicyDecisionRecord, PolicyDecisionStoreError, PolicyInputGeneration,
};
pub use quota::{
    AccountingClass, NewQuotaReservation, QuotaAllocation, QuotaError, QuotaLimit, QuotaLimits, QuotaRepairReport,
    QuotaReservationRecord, QuotaReservationState, QuotaResourceUsage, QuotaUsage, QuotaValue,
};
pub use repository::{
    CreateRepositoryError, DesiredRepository, NewRepository, ReconcileAction, ReconcileRepositoryError,
    ReconciledRepository, RepositoryFieldError, RepositoryId, RepositoryPage, RepositoryQuery, RepositoryQueryError,
    RepositoryRecord, RepositoryState, RepositoryStateError, RepositoryUpdate, UpdateRepositoryError,
};
pub use revocation::{
    DigestRevocation, DigestRevocationPage, DigestRevocationQuery, DigestRevocationQueryError, DigestRevocationState,
    DigestRevocationStatus, LiftRevocationOutcome, PutRevocationError, PutRevocationOutcome,
};
pub use role_grant::{
    CreateGrantOutcome, DeleteGrantOutcome, RoleGrantFilter, RoleGrantOrigin, RoleGrantPage, RoleGrantQuery,
    RoleGrantQueryError, RoleGrantStoreError, StoredRoleGrant, role_grant_reach,
};
pub use scoped_token::{
    NewScopedToken, RevokeScopedTokenOutcome, ScopedTokenPage, ScopedTokenQuery, ScopedTokenQueryError,
    ScopedTokenRecord, ScopedTokenWriteError,
};
pub use user::{StoredPasswordVerifier, UserStoreError};
pub use version::VersionPrecondition;
pub use webhook::{
    NewWebhookDelivery, WebhookDeliveryAttempt, WebhookDeliveryRecord, WebhookDeliveryStatus, WebhookEventIntent,
};

const SERIAL: TableDefinition<&str, u64> = TableDefinition::new("serial");
const WEBHOOK_DELIVERY: TableDefinition<&str, &[u8]> = TableDefinition::new("webhook_delivery");
const WEBHOOK_DUE: TableDefinition<&str, &str> = TableDefinition::new("webhook_due");
const WEBHOOK_EVENT: TableDefinition<&str, &[u8]> = TableDefinition::new("webhook_event");
const JOB_RUN: TableDefinition<&str, &[u8]> = TableDefinition::new("job_run");
const JOB_LEASE: TableDefinition<&str, &[u8]> = TableDefinition::new("job_lease");
/// Operation IDs make admitted-write retries idempotent.
const OPERATION_OUTCOME: TableDefinition<&str, &[u8]> = TableDefinition::new("operation_outcome");
/// Client-scoped keys make staged admissions idempotent and restart-safe.
const INGRESS_INTENT: TableDefinition<&str, &[u8]> = TableDefinition::new("ingress_intent");
const TRANSFER_AUDIT: TableDefinition<&str, &[u8]> = TableDefinition::new("transfer_audit");
/// Per-authority counts bound admission without scanning the intent ledger.
const INGRESS_INTENT_COUNT: TableDefinition<&str, &[u8]> = TableDefinition::new("ingress_intent_count");
/// Admission sequence preserves drain order across restarts.
const INGRESS_INTENT_ORDER: TableDefinition<u64, &str> = TableDefinition::new("ingress_intent_order");
/// Never-reused admission sequence.
const INGRESS_INTENT_SEQ: TableDefinition<&str, u64> = TableDefinition::new("ingress_intent_seq");
const INGRESS_SEQ_KEY: &str = "next";
const RECONCILE_BACKLOG: TableDefinition<&str, &[u8]> = TableDefinition::new("reconcile_backlog");
const POLICY_DECISION: TableDefinition<&str, &[u8]> = TableDefinition::new("policy_decision");
const POLICY_DECISION_CURRENT: TableDefinition<&str, &str> = TableDefinition::new("policy_decision_current");
const POLICY_DECISION_CURRENT_ID: TableDefinition<&str, &str> = TableDefinition::new("policy_decision_current_id");
const POLICY_INPUT_GENERATION: TableDefinition<&str, &[u8]> = TableDefinition::new("policy_input_generation");
const DERIVED_VIEW_FRONTIER: TableDefinition<&str, u64> = TableDefinition::new("derived_view_frontier");
const QUOTA_USAGE: TableDefinition<&str, &[u8]> = TableDefinition::new("quota_usage");
const QUOTA_RESOURCE: TableDefinition<&str, &[u8]> = TableDefinition::new("quota_resource");
const QUOTA_GROUP: TableDefinition<&str, &[u8]> = TableDefinition::new("quota_group");
const QUOTA_BLOB: TableDefinition<&str, &[u8]> = TableDefinition::new("quota_blob");
const QUOTA_RESERVATION: TableDefinition<&str, &[u8]> = TableDefinition::new("quota_reservation");
const QUOTA_ALLOCATION: TableDefinition<&str, &[u8]> = TableDefinition::new("quota_allocation");
const QUOTA_PENDING: TableDefinition<u128, u8> = TableDefinition::new("quota_pending");
const JOURNAL: TableDefinition<u64, &[u8]> = TableDefinition::new("journal");
const WRITER: TableDefinition<&str, &str> = TableDefinition::new("writer");
const JOURNAL_MUTATIONS: TableDefinition<u64, &[u8]> = TableDefinition::new("journal_mutations");
const JOURNAL_BLOBS: TableDefinition<u64, &[u8]> = TableDefinition::new("journal_blobs");
/// Drivers own key and value formats so storage needs no ecosystem-specific tables.
const DRIVER_KV: TableDefinition<&str, &[u8]> = TableDefinition::new("driver_kv");
const ANALYTICS: TableDefinition<&str, &[u8]> = TableDefinition::new("analytics");
const USER: TableDefinition<&str, &[u8]> = TableDefinition::new("server_user");
const USER_NAME: TableDefinition<&str, &str> = TableDefinition::new("server_user_name");
const USER_NAME_SCHEMA: TableDefinition<&str, &str> = TableDefinition::new("server_user_name_schema");
const USER_EVENT: TableDefinition<&str, &[u8]> = TableDefinition::new("server_user_event");
const USER_VERIFIER: TableDefinition<&str, &[u8]> = TableDefinition::new("server_user_verifier");
const ROLE_GRANT: TableDefinition<&str, &[u8]> = TableDefinition::new("role_grant");
const ROLE_GRANT_BY_SCOPE: TableDefinition<&str, &[u8]> = TableDefinition::new("role_grant_by_scope");
const EXTERNAL_IDENTITY: TableDefinition<&[u8], &[u8]> = TableDefinition::new("external_identity");
const EXTERNAL_ROLE_GRANT: TableDefinition<&str, &[u8]> = TableDefinition::new("external_role_grant");
const DIGEST_REVOCATION: TableDefinition<&str, &[u8]> = TableDefinition::new("digest_revocation");
const DIGEST_REVOCATION_STATE: TableDefinition<&str, u64> = TableDefinition::new("digest_revocation_state");
/// Combines source and byte availability to avoid content-store probes during reads.
const ARTIFACT_PLACEMENT: TableDefinition<&str, &[u8]> = TableDefinition::new("artifact_placement");
const BLOB_PLACEMENT: TableDefinition<&str, &[u8]> = TableDefinition::new("blob_placement");
const BLOB_CHUNK_DIGEST: TableDefinition<&str, &[u8]> = TableDefinition::new("blob_chunk_digest");
const RECLAMATION_TOMBSTONE: TableDefinition<&str, &[u8]> = TableDefinition::new("reclamation_tombstone");
const BLOB_RECLAIM_GUARD: TableDefinition<&str, i64> = TableDefinition::new("blob_reclaim_guard");
const REPOSITORY: TableDefinition<&str, &[u8]> = TableDefinition::new("repository");
const REPOSITORY_ROUTE: TableDefinition<&str, &str> = TableDefinition::new("repository_route");
const SCOPED_TOKEN: TableDefinition<&str, &[u8]> = TableDefinition::new("scoped_token");
const SCOPED_TOKEN_REACH: TableDefinition<&str, &str> = TableDefinition::new("scoped_token_reach");
const SCOPED_TOKEN_VERIFIER: TableDefinition<&str, &str> = TableDefinition::new("scoped_token_verifier");
const VISIBILITY_SNAPSHOT: TableDefinition<&str, &[u8]> = TableDefinition::new("visibility_snapshot");
const SERIAL_KEY: &str = "serial";
const WEBHOOK_SERIAL_KEY: &str = "webhook_delivery";
const JOB_SERIAL_KEY: &str = "job_run";
const POLICY_DECISION_SERIAL_KEY: &str = "policy_decision";
const ANALYTICS_KEY: &str = "reads";
const ANALYTICS_DAILY_KEY: &str = "daily_usage";
/// A separate key decouples replica apply-state from producer aggregates.
const ANALYTICS_APPLY_KEY: &str = "apply_state";
/// Durable generation and export watermark prevent duplicate sealed-day exports after restart.
const ANALYTICS_PRODUCER_KEY: &str = "producer";
const VISIBILITY_SNAPSHOT_KEY: &str = "current";
const WRITER_KEY: &str = "active";

/// Opaque driver writes committed atomically through [`MetaStore::commit_driver_batch`].
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

    pub fn put(&mut self, key: String, value: Vec<u8>) {
        self.puts.push((key, value));
    }

    pub fn delete(&mut self, key: String) {
        self.deletes.push(key);
    }
}

#[derive(Debug, Clone)]
pub struct MetaStore {
    db: Arc<MetaDatabase>,
}

enum MetaDatabase {
    ReadWrite(Database),
    ReadOnly(ReadOnlyDatabase),
}

impl std::fmt::Debug for MetaDatabase {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::ReadWrite(_) => "ReadWrite",
            Self::ReadOnly(_) => "ReadOnly",
        })
    }
}

impl MetaDatabase {
    fn begin_read(&self) -> Result<redb::ReadTransaction, redb::TransactionError> {
        match self {
            Self::ReadWrite(db) => db.begin_read(),
            Self::ReadOnly(db) => db.begin_read(),
        }
    }

    fn begin_write(&self) -> Result<redb::WriteTransaction, redb::TransactionError> {
        match self {
            Self::ReadWrite(db) => db.begin_write(),
            Self::ReadOnly(_) => Err(redb::StorageError::from(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "metadata store is read-only",
            ))
            .into()),
        }
    }
}

fn open_optional_table<K: redb::Key + 'static, V: redb::Value + 'static>(
    txn: &redb::ReadTransaction,
    definition: TableDefinition<K, V>,
) -> Result<Option<redb::ReadOnlyTable<K, V>>, MetaError> {
    match txn.open_table(definition) {
        Ok(table) => Ok(Some(table)),
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

impl MetaStore {
    /// Creates shared tables; optional domains create their tables on first use.
    ///
    /// # Errors
    /// Returns a store error if the database cannot be opened or initialized.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, MetaError> {
        Self::initialize(Database::create(path)?)
    }

    fn initialize(db: Database) -> Result<Self, MetaError> {
        let txn = db.begin_write()?;
        {
            txn.open_table(SERIAL)?;
            txn.open_table(WEBHOOK_DELIVERY)?;
            txn.open_table(WEBHOOK_DUE)?;
            txn.open_table(WEBHOOK_EVENT)?;
            txn.open_table(JOB_RUN)?;
            txn.open_table(JOB_LEASE)?;
            txn.open_table(POLICY_DECISION)?;
            txn.open_table(POLICY_DECISION_CURRENT)?;
            txn.open_table(POLICY_DECISION_CURRENT_ID)?;
            txn.open_table(POLICY_INPUT_GENERATION)?;
            txn.open_table(QUOTA_USAGE)?;
            txn.open_table(QUOTA_RESOURCE)?;
            txn.open_table(QUOTA_GROUP)?;
            txn.open_table(QUOTA_BLOB)?;
            txn.open_table(QUOTA_RESERVATION)?;
            txn.open_table(QUOTA_ALLOCATION)?;
            txn.open_table(QUOTA_PENDING)?;
            txn.open_table(DRIVER_KV)?;
            txn.open_table(ANALYTICS)?;
            txn.open_table(USER)?;
            txn.open_table(USER_NAME)?;
            txn.open_table(USER_NAME_SCHEMA)?;
            txn.open_table(USER_EVENT)?;
            txn.open_table(USER_VERIFIER)?;
            txn.open_table(ROLE_GRANT)?;
            txn.open_table(ROLE_GRANT_BY_SCOPE)?;
            txn.open_table(EXTERNAL_IDENTITY)?;
            txn.open_table(EXTERNAL_ROLE_GRANT)?;
            txn.open_table(DIGEST_REVOCATION)?;
            txn.open_table(DIGEST_REVOCATION_STATE)?;
            txn.open_table(OPERATION_OUTCOME)?;
            txn.open_table(REPOSITORY)?;
            txn.open_table(REPOSITORY_ROUTE)?;
            txn.open_table(SCOPED_TOKEN)?;
            txn.open_table(SCOPED_TOKEN_REACH)?;
            txn.open_table(SCOPED_TOKEN_VERIFIER)?;
        }
        user::migrate_names(&txn)?;
        external_identity::backfill_role_grants(&txn)?;
        revocation::backfill_digest_revocation_state(&txn)?;
        txn.commit()?;
        Ok(Self {
            db: Arc::new(MetaDatabase::ReadWrite(db)),
        })
    }

    /// Validates distributed persistence without creating domain tables.
    ///
    /// # Errors
    /// Returns a store error if a write transaction cannot commit.
    pub fn initialize_distributed_state(&self) -> Result<(), MetaError> {
        let txn = self.db.begin_write()?;
        txn.commit()?;
        Ok(())
    }

    /// Rebuilds user records and their name index after Unicode canonicalization data changes.
    ///
    /// # Errors
    /// Returns a collision before writing when two stored users acquire the same canonical name, or
    /// a store error when records cannot be read, rewritten, or committed.
    pub fn migrate_user_names(&self) -> Result<(), MetaError> {
        let txn = self.db.begin_write()?;
        user::migrate_names(&txn)?;
        txn.commit()?;
        Ok(())
    }

    /// # Errors
    /// Returns a store error when the recorded Unicode canonicalization version cannot be read.
    pub fn user_names_require_migration(&self) -> Result<bool, MetaError> {
        let txn = self.db.begin_read()?;
        user::names_require_migration(&txn)
    }

    /// Opens an existing database without creating files or tables.
    ///
    /// # Errors
    /// Returns a store error if the database cannot be opened.
    pub fn open_existing(path: impl AsRef<Path>) -> Result<Self, MetaError> {
        Ok(Self {
            db: Arc::new(MetaDatabase::ReadWrite(Database::open(path)?)),
        })
    }

    /// Opens an existing database without permitting writes or modifying its file.
    ///
    /// # Errors
    /// Returns a store error if the database cannot be opened read-only.
    pub fn open_existing_read_only(path: impl AsRef<Path>) -> Result<Self, MetaError> {
        Ok(Self {
            db: Arc::new(MetaDatabase::ReadOnly(ReadOnlyDatabase::open(path)?)),
        })
    }
}
