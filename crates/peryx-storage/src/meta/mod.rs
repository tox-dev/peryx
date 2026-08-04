//! The metadata store: a redb database holding the monotonic serial counter and the cached
//! upstream simple-index records.
//!
//! redb is a pure-Rust, crash-safe, copy-on-write B-tree with one writer and many readers, so the
//! serial counter and cache records get snapshot-isolated reads without a global lock.

use std::path::Path;
use std::sync::Arc;

use redb::{Database, ReadOnlyDatabase, ReadableDatabase as _, TableDefinition};

mod analytics;
mod blob_placement;
mod bootstrap;
mod cross_dc_copy;
mod error;
mod external_identity;
mod frontier;
mod index;
mod ingress_intent;
mod job;
mod job_lease;
mod journal;
mod operation_outcome;
mod placement;
mod placement_reconcile;
mod policy_decision;
mod quota;
mod reclamation;
mod repository;
mod revocation;
mod role_grant;
mod scoped_token;
mod transfer_attempt;
mod upload_session;
mod user;
mod visibility;
mod webhook;
mod writer;

pub use analytics::AnalyticsHandle;
pub use blob_placement::{
    BackendId, BackendLocation, BlobPlacementError, BlobPlacementFailure, BlobPlacementKey, BlobPlacementOutcome,
    BlobPlacementRecord, BlobPlacementRouting, BlobPlacementState, BlobPlacementStatus, BlobPlacementTransition,
    DataCenterId, MAX_PLACEMENTS_PER_DIGEST, PlacementKeyError,
};
pub use bootstrap::AdministratorBootstrapError;
pub use cross_dc_copy::{
    CopyBacklogEntry, CopyBacklogError, CopyBacklogPage, CopyPlan, CrossDcCopy, MAX_COPY_BACKLOG_BATCH, VerifiedSource,
    plan_cross_dc_copy,
};
pub use error::{MetaError, MetaScanError, WriterIdentityError};
pub use external_identity::ExternalIdentityStoreError;
pub use index::DriverTxn;
pub use ingress_intent::{IntentPhase, IntentStageOutcome, IntentTransition, StagedIntent};
pub use job::{
    FinishJobRun, JobKind, JobOutcome, JobRunPage, JobRunQuery, JobRunQueryError, JobRunRecord, JobRunStoreError,
    JobState, NewJobRun,
};
pub use job_lease::{ClaimOutcome, JobLease, JobLeaseError, LeaseState};
pub use journal::{DriverBlobReference, DriverMutation, JournalRecord, JournalSnapshot};
pub use operation_outcome::{
    OperationClaim, OperationOutcomeError, OperationOutcomeRecord, OperationResult, OperationState,
};
pub use placement::{
    ArtifactOrigin, ArtifactPlacement, ArtifactPlacementHealth, ArtifactPlacementPage, ArtifactPlacementQuery,
    ArtifactPlacementQueryError, ArtifactPlacementRow, ArtifactSource, ByteAvailability, MAX_REPAIR_BATCH,
    PlacementEvent, PlacementRepairPage,
};
pub use placement_reconcile::{
    DigestReconciliation, MAX_PLACEMENT_RECONCILE_BATCH, PlacementReconcileError, PlacementReconcilePage,
};
pub use policy_decision::{
    NewPolicyDecision, PolicyDecisionItem, PolicyDecisionPage, PolicyDecisionQuery, PolicyDecisionQueryError,
    PolicyDecisionRecord, PolicyDecisionStoreError, PolicyInputGeneration,
};
pub use quota::{
    AccountingClass, NewQuotaReservation, QuotaError, QuotaLimit, QuotaLimits, QuotaProjectUsage, QuotaRepairReport,
    QuotaReservationRecord, QuotaReservationState, QuotaUsage, QuotaValue,
};
pub use reclamation::{
    ObservedFrontier, ReadyOutcome, ReclamationError, ReclamationProgress, ReclamationState, ReclamationStatus,
    ReclamationTombstone, SelectOutcome, SkipReason,
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
    CreateGrantOutcome, DeleteGrantOutcome, RoleGrantFilter, RoleGrantPage, RoleGrantQuery, RoleGrantQueryError,
    RoleGrantStoreError, StoredRoleGrant, role_grant_reach,
};
pub use scoped_token::{
    NewScopedToken, RevokeScopedTokenOutcome, ScopedTokenPage, ScopedTokenQuery, ScopedTokenQueryError,
    ScopedTokenRecord,
};
pub use transfer_attempt::{
    AttemptRetention, BeginOutcome, CheckpointOutcome, CheckpointPolicy, MAX_ATTEMPTS_PER_PLACEMENT,
    TransferAttemptError, TransferAttemptMetric, TransferAttemptRecord, TransferAttemptState, TransferAttemptStatus,
    TransferPlan,
};
pub use upload_session::UploadRecord;
pub use user::UserStoreError;
pub use webhook::{NewWebhookDelivery, WebhookDeliveryAttempt, WebhookDeliveryRecord, WebhookDeliveryStatus};

const SERIAL: TableDefinition<&str, u64> = TableDefinition::new("serial");
const WEBHOOK_DELIVERY: TableDefinition<&str, &[u8]> = TableDefinition::new("webhook_delivery");
const WEBHOOK_DUE: TableDefinition<&str, &str> = TableDefinition::new("webhook_due");
const JOB_RUN: TableDefinition<&str, &[u8]> = TableDefinition::new("job_run");
const JOB_LEASE: TableDefinition<&str, &[u8]> = TableDefinition::new("job_lease");
/// The durable outcome of each admitted write, keyed by operation id so a retry replays the original
/// result instead of running a second mutation.
const OPERATION_OUTCOME: TableDefinition<&str, &[u8]> = TableDefinition::new("operation_outcome");
/// The ingress DC's durably staged write intents, keyed by client-scoped identity so a retried
/// admission is idempotent and a restart recovers the intents a home DC has yet to finalize.
const INGRESS_INTENT: TableDefinition<&str, &[u8]> = TableDefinition::new("ingress_intent");
const POLICY_DECISION: TableDefinition<&str, &[u8]> = TableDefinition::new("policy_decision");
const POLICY_DECISION_CURRENT: TableDefinition<&str, &str> = TableDefinition::new("policy_decision_current");
const POLICY_DECISION_CURRENT_ID: TableDefinition<&str, &str> = TableDefinition::new("policy_decision_current_id");
const POLICY_INPUT_GENERATION: TableDefinition<&str, &[u8]> = TableDefinition::new("policy_input_generation");
const DERIVED_VIEW_FRONTIER: TableDefinition<&str, u64> = TableDefinition::new("derived_view_frontier");
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
const USER_VERIFIER: TableDefinition<&str, &[u8]> = TableDefinition::new("server_user_verifier");
const ROLE_GRANT: TableDefinition<&str, &[u8]> = TableDefinition::new("role_grant");
const ROLE_GRANT_BY_SCOPE: TableDefinition<&str, &[u8]> = TableDefinition::new("role_grant_by_scope");
const EXTERNAL_IDENTITY: TableDefinition<&[u8], &[u8]> = TableDefinition::new("external_identity");
const EXTERNAL_ROLE_GRANT: TableDefinition<&str, &[u8]> = TableDefinition::new("external_role_grant");
const DIGEST_REVOCATION: TableDefinition<&str, &[u8]> = TableDefinition::new("digest_revocation");
const DIGEST_REVOCATION_STATE: TableDefinition<&str, u64> = TableDefinition::new("digest_revocation_state");
/// The neutral artifact-placement projection: source and byte availability keyed by content digest,
/// so a package read resolves both dimensions with one indexed lookup and no content-store probe.
const ARTIFACT_PLACEMENT: TableDefinition<&str, &[u8]> = TableDefinition::new("artifact_placement");
const BLOB_PLACEMENT: TableDefinition<&str, &[u8]> = TableDefinition::new("blob_placement");
/// The durable transfer attempts populating blob placements: one current attempt and a bounded retry
/// history per `(digest, backend, data center, location)`, keyed by placement then attempt sequence.
const TRANSFER_ATTEMPT: TableDefinition<&str, &[u8]> = TableDefinition::new("transfer_attempt");
/// In-progress chunked blob upload sessions, keyed by session id so a restart resumes an upload from its
/// last staged offset.
const UPLOAD_SESSION: TableDefinition<&str, &[u8]> = TableDefinition::new("upload_session");
const RECLAMATION_TOMBSTONE: TableDefinition<&str, &[u8]> = TableDefinition::new("reclamation_tombstone");
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
const ANALYTICS_KEY: &str = "downloads";
const ANALYTICS_DAILY_KEY: &str = "daily_usage";
const VISIBILITY_SNAPSHOT_KEY: &str = "current";
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
            txn.open_table(JOB_LEASE)?;
            txn.open_table(POLICY_DECISION)?;
            txn.open_table(POLICY_DECISION_CURRENT)?;
            txn.open_table(POLICY_DECISION_CURRENT_ID)?;
            txn.open_table(POLICY_INPUT_GENERATION)?;
            txn.open_table(DERIVED_VIEW_FRONTIER)?;
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
            txn.open_table(VISIBILITY_SNAPSHOT)?;
            txn.open_table(USER)?;
            txn.open_table(USER_NAME)?;
            txn.open_table(USER_EVENT)?;
            txn.open_table(USER_VERIFIER)?;
            txn.open_table(ROLE_GRANT)?;
            txn.open_table(ROLE_GRANT_BY_SCOPE)?;
            txn.open_table(EXTERNAL_IDENTITY)?;
            txn.open_table(EXTERNAL_ROLE_GRANT)?;
            txn.open_table(DIGEST_REVOCATION)?;
            txn.open_table(DIGEST_REVOCATION_STATE)?;
            txn.open_table(ARTIFACT_PLACEMENT)?;
            txn.open_table(BLOB_PLACEMENT)?;
            txn.open_table(TRANSFER_ATTEMPT)?;
            txn.open_table(UPLOAD_SESSION)?;
            txn.open_table(RECLAMATION_TOMBSTONE)?;
            txn.open_table(OPERATION_OUTCOME)?;
            txn.open_table(INGRESS_INTENT)?;
            txn.open_table(REPOSITORY)?;
            txn.open_table(REPOSITORY_ROUTE)?;
            txn.open_table(SCOPED_TOKEN)?;
            txn.open_table(SCOPED_TOKEN_REACH)?;
            txn.open_table(SCOPED_TOKEN_VERIFIER)?;
        }
        txn.commit()?;
        Ok(Self {
            db: Arc::new(MetaDatabase::ReadWrite(db)),
        })
    }

    /// Open an existing database without creating files or tables.
    ///
    /// # Errors
    /// Returns a store error if the database cannot be opened.
    pub fn open_existing(path: impl AsRef<Path>) -> Result<Self, MetaError> {
        Ok(Self {
            db: Arc::new(MetaDatabase::ReadWrite(Database::open(path)?)),
        })
    }

    /// Open an existing database without permitting writes or modifying its file.
    ///
    /// # Errors
    /// Returns a store error if the database cannot be opened read-only.
    pub fn open_existing_read_only(path: impl AsRef<Path>) -> Result<Self, MetaError> {
        Ok(Self {
            db: Arc::new(MetaDatabase::ReadOnly(ReadOnlyDatabase::open(path)?)),
        })
    }
}
