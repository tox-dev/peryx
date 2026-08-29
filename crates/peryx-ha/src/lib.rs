//! Availability contracts independent of deployment mode.

mod blob;
mod placement;
mod reclamation;
mod reconcile;
mod store;
mod views;

pub use blob::{
    BlobAvailability, BlobAvailabilityError, BlobAvailabilityFailure, BlobServices, BlobWriteDurability, CommittedBlob,
    WriteDurability,
};
pub use placement::{
    ArtifactOrigin, ArtifactPlacement, ArtifactPlacementHealth, ArtifactPlacementPage, ArtifactPlacementQuery,
    ArtifactPlacementRow, ArtifactSource, BackendId, BackendLocation, BlobPlacementDecisionError, BlobPlacementFailure,
    BlobPlacementGroupPage, BlobPlacementKey, BlobPlacementOutcome, BlobPlacementPage, BlobPlacementRecord,
    BlobPlacementRouting, BlobPlacementState, BlobPlacementStatus, BlobPlacementTransition, ByteAvailability,
    DataCenterId, MAX_PLACEMENTS_PER_DIGEST, MAX_REPAIR_BATCH, PlacementEvent, PlacementKeyError, PlacementRepairPage,
    decide_blob_placement,
};
pub use reclamation::{
    ReadyOutcome, ReclaimGuard, ReclaimGuardArm, ReclamationDecisionError, ReclamationProgress, ReclamationSnapshot,
    ReclamationState, ReclamationStatus, ReclamationTombstone, SelectOutcome, SkipReason, decide_reclamation_readiness,
    decide_reclamation_selection,
};
pub use reconcile::{NewReconcileEntry, ReconcileEnqueue, ReconcileEntry, ReconcilePage};
pub use store::{
    ArtifactPlacementStore, BlobPlacementStore, CompareWrite, ReclaimGuardStore, ReclamationStore, ReconcileStore,
    TransferAudit, TransferAuditStore, VisibilitySnapshotStore,
};
pub use views::{
    AvailabilityPageQuery, AvailabilityViewReader, BlobPlacementViewError, OperationsViewError, PlacementViewError,
};

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
use std::sync::Arc;

use async_trait::async_trait;
pub use peryx_core::{
    AnalyticsSnapshotStore, AvailabilityReadError, BlobDurability, BlobMetadata, Digest, DurabilityRequirement,
    JournalCommit, NodeRole, ObservedFrontier, PrometheusSource, TopologyConfig,
};
use serde::{Deserialize, Serialize};

pub const AVAILABILITY_BLOB_VIEW: &str = "blob";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AvailabilityMode {
    #[default]
    None,
    Dc,
    Ha,
}

impl AvailabilityMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Dc => "dc",
            Self::Ha => "ha",
        }
    }

    #[must_use]
    pub const fn durability_requirement(self) -> DurabilityRequirement {
        match self {
            Self::None => DurabilityRequirement::LOCAL,
            Self::Dc | Self::Ha => DurabilityRequirement::REPLICATED,
        }
    }

    #[must_use]
    pub const fn is_distributed(self) -> bool {
        matches!(self, Self::Dc | Self::Ha)
    }

    #[must_use]
    pub const fn availability_resources(self) -> AvailabilityResources {
        match self {
            Self::None => AvailabilityResources::None,
            Self::Dc | Self::Ha => AvailabilityResources::Distributed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvailabilityResources {
    None,
    Distributed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AvailabilityAudience {
    Public,
    Operator,
    Administrator,
}

#[async_trait]
pub trait AvailabilityAuthorizer: Send + Sync {
    async fn authorize(&self, authorization: Option<&str>) -> AvailabilityAudience;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontierReply {
    pub epoch: u64,
    pub applied_frontier: u64,
}

#[async_trait]
pub trait MetadataFrontierProvider: Send + Sync {
    async fn frontier(&self, authority: &str) -> Option<FrontierReply>;
}

pub struct AvailabilityInstall<Routes> {
    pub role: NodeRole,
    pub topology: TopologyConfig,
    pub blobs: BlobServices,
    pub analytics: Arc<dyn AnalyticsCompleteness>,
    pub operations: Arc<dyn OperationObserver>,
    pub capabilities: AvailabilityCapabilities,
    pub authority_drainer: Option<Arc<dyn AuthorityDrainer>>,
    pub metrics: Vec<Arc<dyn PrometheusSource>>,
    pub routes: Routes,
}

pub struct AvailabilityStateInstall {
    pub role: NodeRole,
    pub topology: TopologyConfig,
    pub blobs: BlobServices,
    pub analytics: Arc<dyn AnalyticsCompleteness>,
    pub capabilities: AvailabilityCapabilities,
    pub authority_drainer: Option<Arc<dyn AuthorityDrainer>>,
    pub operations: Option<Arc<dyn OperationObserver>>,
}

#[derive(Default)]
pub struct AvailabilityCapabilities {
    pub ownership: Option<Arc<dyn OwnershipAuthority>>,
    pub copier: Option<Arc<dyn CrossDcCopier>>,
    pub placement: Option<Arc<dyn PlacementReconciler>>,
    pub home_placement: Option<Arc<dyn HomePlacementRecorder>>,
    pub reclaimer: Option<Arc<dyn BlobReclaimer>>,
}

pub trait HomePlacementRecorder: Send + Sync {
    /// # Errors
    /// Returns an error when the digest is invalid or the placement cannot be persisted.
    fn record(&self, digest: &str, size: u64, fence: u64) -> Result<(), String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvailabilityShutdownStage {
    Listener,
    Consensus,
    Runtime,
}

#[derive(Debug)]
pub struct AvailabilityShutdownFailure {
    pub stage: AvailabilityShutdownStage,
    pub source: Box<dyn Error + Send + Sync>,
}

#[derive(Debug)]
pub struct AvailabilityShutdownError {
    failures: Vec<AvailabilityShutdownFailure>,
}

impl AvailabilityShutdownError {
    #[must_use]
    pub fn new(stage: AvailabilityShutdownStage, source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            failures: vec![AvailabilityShutdownFailure {
                stage,
                source: Box::new(source),
            }],
        }
    }

    pub fn push(&mut self, stage: AvailabilityShutdownStage, source: impl Error + Send + Sync + 'static) {
        self.failures.push(AvailabilityShutdownFailure {
            stage,
            source: Box::new(source),
        });
    }

    #[must_use]
    pub fn failures(&self) -> &[AvailabilityShutdownFailure] {
        &self.failures
    }
}

impl fmt::Display for AvailabilityShutdownError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("availability shutdown failed")?;
        for failure in &self.failures {
            write!(formatter, "; {:?}: {}", failure.stage, failure.source)?;
        }
        Ok(())
    }
}

impl Error for AvailabilityShutdownError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.failures[0].source.as_ref())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AvailabilityFailure {
    message: String,
}

impl AvailabilityFailure {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for AvailabilityFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for AvailabilityFailure {}

#[async_trait]
pub trait AvailabilityHandle: Send {
    type Active: ActiveAvailabilityHandle;
    type Error;

    #[expect(clippy::missing_errors_doc, reason = "implementations define backend errors")]
    fn activate(self) -> Result<Self::Active, Self::Error>;

    /// # Errors
    ///
    /// Returns all failures reported while stopping owned availability resources.
    async fn shutdown(self) -> Result<(), AvailabilityShutdownError>;
}

#[async_trait]
pub trait ActiveAvailabilityHandle: Send {
    async fn wait_for_failure(&mut self) -> AvailabilityFailure;

    /// # Errors
    ///
    /// Returns all failures reported while stopping active availability resources.
    async fn shutdown(&mut self) -> Result<(), AvailabilityShutdownError>;
}

pub struct PreparedAvailability<Routes, Handle> {
    pub public_routes: Routes,
    pub private_routes: Option<Routes>,
    pub metrics: Vec<Arc<dyn PrometheusSource>>,
    pub is_replica: bool,
    pub handle: Handle,
}

impl<Routes, Handle: AvailabilityHandle> PreparedAvailability<Routes, Handle> {
    /// # Errors
    ///
    /// Returns the backend startup error after rolling back resources created by the transition.
    pub fn activate(self) -> Result<ActiveAvailability<Handle::Active>, Handle::Error> {
        self.handle.activate().map(|handle| ActiveAvailability { handle })
    }

    /// # Errors
    ///
    /// Returns all failures reported while stopping owned availability resources.
    pub async fn shutdown(self) -> Result<(), AvailabilityShutdownError> {
        self.handle.shutdown().await
    }
}

pub struct ActiveAvailability<Handle> {
    pub handle: Handle,
}

pub trait AvailabilityAssembler {
    type Config;
    type Context;
    type Routes;
    type Error;

    /// # Errors
    ///
    /// Fails when configuration or required availability resources cannot be initialized.
    fn assemble(
        config: &Self::Config,
        context: &Self::Context,
    ) -> Result<AvailabilityInstall<Self::Routes>, Self::Error>;
}

#[async_trait]
pub trait AvailabilityRuntime: Sized {
    type Context;
    type Routes;
    type PreparedHandle;
    type Error;

    #[must_use]
    fn routes(&self) -> Self::Routes;

    #[must_use]
    fn metrics(&self) -> Vec<Arc<dyn PrometheusSource>>;

    async fn prepare(
        self,
        context: Self::Context,
    ) -> Result<PreparedAvailability<Self::Routes, Self::PreparedHandle>, Self::Error>;
}

impl AvailabilityResources {
    #[must_use]
    pub const fn has_distributed_state(self) -> bool {
        matches!(self, Self::Distributed)
    }

    #[must_use]
    pub const fn has_routes(self) -> bool {
        matches!(self, Self::Distributed)
    }

    #[must_use]
    pub const fn has_metrics(self) -> bool {
        matches!(self, Self::Distributed)
    }

    #[must_use]
    pub const fn has_background_tasks(self) -> bool {
        matches!(self, Self::Distributed)
    }

    #[must_use]
    pub const fn replica_derived_view(self, is_replica: bool) -> Option<&'static str> {
        if matches!(self, Self::Distributed) && is_replica {
            Some(AVAILABILITY_BLOB_VIEW)
        } else {
            None
        }
    }
}

/// Stable analytics producer identity.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProducerId(pub String);

/// Idempotency key for an analytics producer interval.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct IntervalId {
    pub producer: ProducerId,
    pub epoch: AuthorityEpoch,
    pub sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AggregateKey {
    pub day: i64,
    pub repository: String,
    pub resource: String,
    pub group: String,
    pub source: String,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AggregateDelta {
    pub downloads: u64,
    pub bytes: u64,
}

impl AggregateDelta {
    #[must_use]
    pub const fn saturating_add(self, other: Self) -> Self {
        Self {
            downloads: self.downloads.saturating_add(other.downloads),
            bytes: self.bytes.saturating_add(other.bytes),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AggregateRow {
    pub key: AggregateKey,
    pub delta: AggregateDelta,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalyticsBatch {
    pub interval: IntervalId,
    pub rows: Vec<AggregateRow>,
}

pub trait AnalyticsBatchSource: Send + Sync {
    fn sealed_batches(&self, producer: &ProducerId, epoch: AuthorityEpoch, after_day: i64) -> Vec<AnalyticsBatch>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completeness {
    Complete,
    Delayed,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletenessQuery {
    pub from_day: i64,
    pub to_day: i64,
    pub today: i64,
    pub repository: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedProducer {
    pub producer: ProducerId,
    pub dc: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProducerReport {
    pub producer: ProducerId,
    pub dc: String,
    pub accepted: Option<(AuthorityEpoch, u64)>,
    pub state: Completeness,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DayBucket {
    pub day: i64,
    pub downloads: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletenessReport {
    pub completeness: Completeness,
    pub frontier_day: Option<i64>,
    pub required_day: Option<i64>,
    pub lag_days: Option<i64>,
    pub producers: Vec<ProducerReport>,
    pub buckets: Vec<DayBucket>,
    pub totals: AggregateDelta,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("distributed analytics are unavailable")]
pub struct CompletenessError;

pub trait AnalyticsCompleteness: Send + Sync {
    /// # Errors
    ///
    /// Returns [`CompletenessError`] when distributed analytics cannot be read.
    fn assess(
        &self,
        store: &dyn AnalyticsSnapshotStore,
        expected: &[ExpectedProducer],
        query: &CompletenessQuery,
    ) -> Result<CompletenessReport, CompletenessError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlCommand {
    AddLearner {
        datacenter: String,
        address: String,
    },
    PromoteVoter {
        datacenter: String,
    },
    RemoveVoter {
        datacenter: String,
    },
    ReplaceVoter {
        remove: String,
        datacenter: String,
        address: String,
    },
    TransferAuthority {
        authority: String,
        new_home: String,
    },
    AdvanceEpoch {
        authority: String,
    },
}

impl ControlCommand {
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::AddLearner { .. } => "add_learner",
            Self::PromoteVoter { .. } => "promote_voter",
            Self::RemoveVoter { .. } => "remove_voter",
            Self::ReplaceVoter { .. } => "replace_voter",
            Self::TransferAuthority { .. } => "transfer_authority",
            Self::AdvanceEpoch { .. } => "advance_epoch",
        }
    }

    #[must_use]
    pub fn target(&self) -> &str {
        match self {
            Self::AddLearner { datacenter, .. }
            | Self::PromoteVoter { datacenter }
            | Self::RemoveVoter { datacenter }
            | Self::ReplaceVoter { datacenter, .. } => datacenter,
            Self::TransferAuthority { authority, .. } | Self::AdvanceEpoch { authority } => authority,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommandReceipt {
    pub term: u64,
    pub index: u64,
    pub outcome: CommandOutcome,
    pub old_voters: Vec<String>,
    pub new_voters: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandOutcome {
    Committed,
    NoChange,
}

impl CommandOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Committed => "committed",
            Self::NoChange => "no_change",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ControlError {
    #[error("not the consensus leader{}", .leader.as_deref().map(|address| format!("; leader at {address}")).unwrap_or_default())]
    NotLeader { leader: Option<String> },
    #[error("consensus command did not commit: {0}")]
    Unavailable(String),
    #[error("invalid command: {0}")]
    Invalid(String),
    #[error("too many concurrent availability commands in flight")]
    Overloaded,
    #[error("idempotency key already used for a different command")]
    KeyReuse,
}

impl ControlError {
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::NotLeader { .. } => "not_leader",
            Self::Unavailable(_) => "unavailable",
            Self::Invalid(_) => "invalid",
            Self::Overloaded => "overloaded",
            Self::KeyReuse => "key_reuse",
        }
    }
}

#[async_trait]
pub trait MembershipControl: Send + Sync {
    /// # Errors
    ///
    /// Returns [`ControlError`] when the command is invalid, rejected, overloaded, or cannot commit.
    async fn submit(&self, command: ControlCommand) -> Result<CommandReceipt, ControlError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ControlMetrics {
    pub completed: u64,
    pub p50_ms: i64,
    pub p99_ms: i64,
}

#[async_trait]
pub trait ControlExecutor: Send + Sync {
    /// # Errors
    ///
    /// Returns [`ControlError`] when authorization or command execution fails.
    async fn execute(
        &self,
        actor: &str,
        key: Option<&str>,
        command: ControlCommand,
    ) -> Result<CommandReceipt, ControlError>;

    fn metrics(&self) -> ControlMetrics;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlActor(String);

impl ControlActor {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ControlActor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlPermission {
    Read,
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("control identity provider unavailable")]
pub struct ControlAuthenticationError;

#[async_trait]
pub trait ControlAuthorizer: Send + Sync {
    /// # Errors
    ///
    /// Returns [`ControlAuthenticationError`] when the identity provider is unavailable.
    async fn authenticate(
        &self,
        authorization: Option<&str>,
    ) -> Result<Option<ControlActor>, ControlAuthenticationError>;

    fn allows(&self, actor: &ControlActor, permission: ControlPermission) -> bool;
}

#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeClaim {
    /// The datacenter selected by the committed assignment.
    pub home: String,
    /// The assignment epoch the winner must re-admit before publication.
    pub epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferOutcome {
    pub from: String,
    pub to: String,
    pub epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ClusterStatus {
    pub leader: Option<String>,
    pub term: u64,
    pub voters: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum OwnershipError {
    #[error("not the ownership leader{}", .leader.as_deref().map(|address| format!("; leader at {address}")).unwrap_or_default())]
    NotLeader { leader: Option<String> },
    #[error("ownership claim did not commit: {0}")]
    Unavailable(String),
}

#[async_trait]
pub trait OwnershipAuthority: Send + Sync {
    /// # Errors
    ///
    /// Returns [`OwnershipError`] when the claim cannot commit on the ownership leader.
    async fn claim_home(&self, authority: &str) -> Result<HomeClaim, OwnershipError>;

    fn cluster_status(&self) -> ClusterStatus;

    async fn committed_epoch(&self, authority: &str) -> u64;

    async fn admit_epoch(&self, authority: &str, presented: u64) -> bool;

    /// # Errors
    ///
    /// Returns [`OwnershipError`] when the transfer cannot commit on the ownership leader.
    async fn transfer_home(&self, authority: &str, new_home: &str) -> Result<Option<TransferOutcome>, OwnershipError>;
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AvailabilityTaskReport {
    pub processed: u64,
    pub changed: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailabilityTaskError {
    code: &'static str,
    message: String,
}

impl AvailabilityTaskError {
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.code
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for AvailabilityTaskError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for AvailabilityTaskError {}

#[async_trait]
pub trait CrossDcCopier: Send + Sync {
    /// # Errors
    ///
    /// Returns [`AvailabilityTaskError`] when a copy pass cannot complete.
    async fn copy_pass(
        &self,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
        concurrency: NonZeroUsize,
    ) -> Result<AvailabilityTaskReport, AvailabilityTaskError>;
}

#[async_trait]
pub trait PlacementReconciler: Send + Sync {
    /// # Errors
    ///
    /// Returns [`AvailabilityTaskError`] when a reconciliation pass cannot complete.
    async fn reconcile_pass(
        &self,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
        batch: NonZeroUsize,
    ) -> Result<AvailabilityTaskReport, AvailabilityTaskError>;
}

#[async_trait]
pub trait BlobReclaimer: Send + Sync {
    /// # Errors
    ///
    /// Returns [`AvailabilityTaskError`] when a reclamation pass cannot complete.
    async fn reclaim_pass(
        &self,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
        fence: u64,
        batch: NonZeroUsize,
    ) -> Result<AvailabilityTaskReport, AvailabilityTaskError>;
}

#[async_trait]
pub trait AuthorityDrainer: Send + Sync {
    /// # Errors
    ///
    /// Returns [`AvailabilityTaskError`] when authority work cannot drain.
    async fn drain(
        &self,
        now: i64,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<AvailabilityTaskReport, AvailabilityTaskError>;
}

pub trait ReferenceInventory: Send + Sync {
    /// # Errors
    ///
    /// Returns the inventory error without changing reclamation state.
    fn referenced(&self) -> Result<BTreeSet<String>, String>;
}

pub trait ReclamationFrontiers: Send + Sync {
    fn observe(&self) -> Option<ObservedFrontier>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplicaPage {
    pub changes: usize,
    pub serial: u64,
    pub primary_serial: u64,
}

#[derive(Clone)]
pub struct AppliedFrontier {
    sender: tokio::sync::watch::Sender<u64>,
}

impl Default for AppliedFrontier {
    fn default() -> Self {
        let (sender, _) = tokio::sync::watch::channel(0);
        Self { sender }
    }
}

impl AppliedFrontier {
    #[must_use]
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.sender.subscribe()
    }

    pub fn publish(&self, serial: u64) {
        self.sender.send_replace(serial);
    }
}

pub trait ReplicaViewApplier: Send + Sync {
    fn apply(&self, page: ReplicaPage, changed_keys: &[String]);
    fn readable_frontier(&self) -> u64;
    fn publish_applied_frontier(&self, serial: u64);
}

/// The authority generation that fences stale writers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AuthorityEpoch(pub u64);

/// A replicated mutation's stable class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OperationKind {
    Publish,
    Withdraw,
    Delete,
    CacheFill,
    Visibility,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationObservation {
    pub source: String,
    pub epoch: AuthorityEpoch,
    pub serial: u64,
    pub kind: OperationKind,
}

pub trait OperationObserver: Send + Sync {
    fn record(&self, operation: OperationObservation);
}

impl OperationKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Publish => "publish",
            Self::Withdraw => "withdraw",
            Self::Delete => "delete",
            Self::CacheFill => "cache-fill",
            Self::Visibility => "visibility",
        }
    }
}

impl std::fmt::Display for OperationKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The number of members that must hold a mutation before it is durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurabilityPolicy {
    Local,
    Majority,
    Everywhere,
    AtLeast(NonZeroUsize),
}

impl DurabilityPolicy {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Majority => "majority",
            Self::Everywhere => "everywhere",
            Self::AtLeast(_) => "at_least",
        }
    }

    #[must_use]
    pub const fn required_acks(self, configured: usize) -> usize {
        match self {
            Self::Local => 1,
            Self::Majority => configured / 2 + 1,
            Self::Everywhere => configured,
            Self::AtLeast(acks) => acks.get(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    #[error("peer connection dropped before the batch completed")]
    Disconnected,
    #[error("peer did not answer within the transfer deadline")]
    Timeout,
    #[error("peer rejected the replication credential")]
    Unauthenticated,
    #[error("peer replication endpoint returned transient server error {status}")]
    ServerError { status: u16 },
    #[error("peer replication endpoint returned status {status}")]
    BadStatus { status: u16 },
    #[error("peer replication reply was not a valid change page")]
    Malformed,
    #[error("batch frame is {actual} bytes; the transport caps a frame at {limit}")]
    FrameTooLarge { limit: u64, actual: u64 },
    #[error("request asked for {actual} operations; the peer caps a batch at {limit}")]
    TooManyOperations { limit: usize, actual: usize },
    #[error("peer advertised source {actual:?}; the frontier follows {expected:?}")]
    SourceChanged { expected: String, actual: String },
    #[error("peer batch starts at serial {actual}; the frontier is at {expected}")]
    FrontierGap { expected: u64, actual: u64 },
    #[error("peer returned no changes but advertised frontier {frontier} past serial {after}")]
    EmptyBatch { frontier: u64, after: u64 },
    #[error("peer blob content hashes to {actual}; the request asked for {expected}")]
    DigestMismatch { expected: String, actual: String },
    #[error("peer holds no blob for digest {digest}")]
    BlobNotFound { digest: String },
    #[error("peer blob transport is at its concurrent-stream limit")]
    AtCapacity,
}

impl TransportError {
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Disconnected | Self::Timeout | Self::ServerError { .. } | Self::AtCapacity
        )
    }

    #[must_use]
    pub const fn terminal_reason(&self) -> Option<&'static str> {
        match self {
            Self::Disconnected | Self::Timeout | Self::ServerError { .. } | Self::AtCapacity => None,
            Self::Unauthenticated => Some("unauthenticated"),
            Self::DigestMismatch { .. } => Some("digest_mismatch"),
            Self::BlobNotFound { .. } => Some("blob_not_found"),
            Self::BadStatus { .. } => Some("bad_status"),
            Self::Malformed => Some("malformed"),
            Self::FrameTooLarge { .. } => Some("frame_too_large"),
            Self::TooManyOperations { .. } => Some("too_many_operations"),
            Self::SourceChanged { .. } => Some("source_changed"),
            Self::FrontierGap { .. } => Some("frontier_gap"),
            Self::EmptyBatch { .. } => Some("empty_batch"),
        }
    }

    /// Source and frontier protocol violations require an operator to restore trust.
    #[must_use]
    pub const fn requires_explicit_rearm(&self) -> bool {
        matches!(
            self,
            Self::SourceChanged { .. } | Self::FrontierGap { .. } | Self::EmptyBatch { .. }
        )
    }
}

/// Proof that one peer durably holds an artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerReceipt {
    pub node: String,
    pub digest: Digest,
    pub size: u64,
}

#[async_trait]
pub trait ReceiptSource: Sync {
    fn node(&self) -> &str;

    /// # Errors
    ///
    /// Returns [`TransportError`] when the peer request or response fails.
    async fn fetch_receipt(&self, digest: &Digest) -> Result<Option<PeerReceipt>, TransportError>;
}

/// A remote datacenter's accepted metadata frontier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAck {
    pub datacenter: String,
    pub epoch: u64,
    pub applied_frontier: u64,
}

#[async_trait]
pub trait RemoteFrontierSource: Sync {
    fn datacenter(&self) -> &str;

    /// # Errors
    ///
    /// Returns [`TransportError`] when the peer request or response fails.
    async fn fetch_frontier(&self, authority: &str) -> Result<Option<RemoteAck>, TransportError>;
}

/// The metadata position an HA write must prove remotely durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetadataOperation {
    pub epoch: u64,
    pub frontier: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct WriteAckRequest<'a> {
    pub digest: &'a Digest,
    pub authority: &'a str,
    pub operation: MetadataOperation,
}

/// A byte-quorum decision with its counted nodes and configured threshold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ByteAckDecision {
    Acknowledged {
        nodes: Vec<String>,
        required: usize,
    },
    Pending {
        nodes: Vec<String>,
        required: usize,
        remaining: usize,
    },
}

impl ByteAckDecision {
    #[must_use]
    pub const fn is_acknowledged(&self) -> bool {
        matches!(self, Self::Acknowledged { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ByteEvidence {
    Filesystem(ByteAckDecision),
    ObjectStore { acknowledged: bool },
}

impl ByteEvidence {
    #[must_use]
    pub const fn is_durable(&self) -> bool {
        match self {
            Self::Filesystem(decision) => decision.is_acknowledged(),
            Self::ObjectStore { acknowledged } => *acknowledged,
        }
    }

    #[must_use]
    pub const fn scope(&self) -> BlobDurability {
        match self {
            Self::Filesystem(_) => BlobDurability::Filesystem,
            Self::ObjectStore { .. } => BlobDurability::ObjectStore,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deadline {
    Live,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DcAck {
    Durable { scope: BlobDurability },
    Pending,
    Unknown,
}

pub trait WriteAckObserver: Send + Sync {
    fn record(&self, outcome: DcAck, byte_decision: &ByteAckDecision);
}

#[cfg(test)]
#[path = "../tests/unit/tests.rs"]
mod tests;
