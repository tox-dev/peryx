//! Availability contracts independent of deployment mode.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;

use async_trait::async_trait;
use peryx_core::{TopologyConfig, TopologyMember, TopologySnapshot};
use peryx_storage::blob::{BlobDurability, BlobError, BlobMetadata, BlobStorage, Digest, DurabilityRequirement};
use peryx_storage::meta::{MetaError, MetaStore};
use serde::{Deserialize, Serialize};

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
    pub const fn is_dc(self) -> bool {
        matches!(self, Self::Dc)
    }

    #[must_use]
    pub const fn is_ha(self) -> bool {
        matches!(self, Self::Ha)
    }
}

/// Stable identity of an analytics producer.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProducerId(pub String);

/// Idempotency key for one producer interval.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct IntervalId {
    pub producer: ProducerId,
    pub epoch: AuthorityEpoch,
    pub sequence: u64,
}

/// Dimensions of one replicated analytics aggregate.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AggregateKey {
    pub day: i64,
    pub repository: String,
    pub project: String,
    pub version: String,
    pub source: String,
}

/// Additive totals for an analytics aggregate.
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

/// One aggregate carried by an analytics batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AggregateRow {
    pub key: AggregateKey,
    pub delta: AggregateDelta,
}

/// One idempotent analytics interval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnalyticsBatch {
    pub interval: IntervalId,
    pub rows: Vec<AggregateRow>,
}

/// Coverage of a distributed analytics result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completeness {
    Complete,
    Delayed,
    Unavailable,
}

/// Bounded inputs for a distributed analytics completeness query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletenessQuery {
    pub from_day: i64,
    pub to_day: i64,
    pub today: i64,
    pub repository: Option<String>,
}

/// One producer expected in a completeness result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedProducer {
    pub producer: ProducerId,
    pub dc: String,
}

/// One producer's accepted coverage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProducerReport {
    pub producer: ProducerId,
    pub dc: String,
    pub accepted: Option<(AuthorityEpoch, u64)>,
    pub state: Completeness,
}

/// One day's converged analytics totals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DayBucket {
    pub day: i64,
    pub downloads: u64,
    pub bytes: u64,
}

/// Distributed analytics coverage and totals.
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

/// Failure to restore the distributed analytics view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("distributed analytics are unavailable")]
pub struct CompletenessError;

/// Reads a converged analytics view without exposing its implementation.
pub trait AnalyticsCompleteness: Send + Sync {
    /// # Errors
    /// Returns an error when the converged view is unavailable.
    fn assess(
        &self,
        meta: &MetaStore,
        expected: &[ExpectedProducer],
        query: &CompletenessQuery,
    ) -> Result<CompletenessReport, CompletenessError>;
}

/// Current membership without prescribing discovery or consensus.
pub trait MembershipProvider: Send + Sync {
    fn members(&self) -> &[TopologyMember];
}

/// Authority lease visible to mutation admission.
pub trait Lease: Send + Sync {
    fn holder(&self) -> Option<&str>;
}

/// Persistence boundary for the current topology view.
#[async_trait]
pub trait TopologyStore: Send + Sync {
    type Error: std::error::Error + Send + Sync + 'static;

    async fn load(&self) -> Result<TopologySnapshot, Self::Error>;
    async fn store(&self, topology: &TopologySnapshot) -> Result<(), Self::Error>;
}

/// Runtime availability posture used by startup and diagnostics.
pub trait HaCoordinator: Send + Sync {
    fn configuration(&self) -> TopologyConfig;
    fn topology(&self, captured_at: i64) -> TopologySnapshot;
    fn distributed(&self) -> bool;
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
    async fn submit(&self, command: ControlCommand) -> Result<CommandReceipt, ControlError>;
}

#[must_use]
pub fn plan_voter_roster(current: &BTreeSet<u64>, add: Option<u64>, remove: Option<u64>) -> BTreeSet<u64> {
    let mut roster = current.clone();
    if let Some(id) = add {
        roster.insert(id);
    }
    if let Some(id) = remove {
        roster.remove(&id);
    }
    roster
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HomeClaim {
    AssignedHere,
    AlreadyHomed,
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
    async fn has_home(&self, authority: &str) -> bool;

    async fn claim_home(&self, authority: &str) -> Result<HomeClaim, OwnershipError>;

    fn cluster_status(&self) -> ClusterStatus;

    async fn committed_epoch(&self, authority: &str) -> u64;

    async fn admit_epoch(&self, authority: &str, presented: u64) -> bool;

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
    async fn copy_pass(
        &self,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
        concurrency: NonZeroUsize,
    ) -> Result<AvailabilityTaskReport, AvailabilityTaskError>;
}

#[async_trait]
pub trait PlacementReconciler: Send + Sync {
    async fn reconcile_pass(
        &self,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
        batch: NonZeroUsize,
    ) -> Result<AvailabilityTaskReport, AvailabilityTaskError>;
}

#[async_trait]
pub trait BlobReclaimer: Send + Sync {
    async fn reclaim_pass(
        &self,
        cancelled: &(dyn Fn() -> bool + Send + Sync),
        fence: u64,
        batch: NonZeroUsize,
    ) -> Result<AvailabilityTaskReport, AvailabilityTaskError>;
}

/// Result of attempting to fetch a locally missing blob from distributed storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadThroughOutcome {
    Served,
    Unavailable,
}

/// A local failure while staging a remotely fetched blob.
#[derive(Debug, thiserror::Error)]
pub enum ReadThroughError {
    #[error("read-through could not read blob placements: {0}")]
    Meta(#[source] MetaError),
    #[error("read-through could not stage the fetched blob: {0}")]
    Blob(#[source] BlobError),
}

/// Distributed blob retrieval behind a deployment-independent serving boundary.
#[async_trait]
pub trait RemoteBlobReader: Send + Sync {
    async fn read_through(
        &self,
        meta: &MetaStore,
        blobs: &BlobStorage,
        digest: &Digest,
    ) -> Result<ReadThroughOutcome, ReadThroughError>;
}

/// Populate a missing local blob when distributed read-through is installed.
pub async fn fill_from_remote_placement(
    reader: Option<&dyn RemoteBlobReader>,
    meta: &MetaStore,
    blobs: &BlobStorage,
    digest: &Digest,
) -> Option<BlobMetadata> {
    let reader = reader?;
    match reader.read_through(meta, blobs, digest).await {
        Ok(ReadThroughOutcome::Served) => blobs.head(digest).await.ok().flatten(),
        Ok(ReadThroughOutcome::Unavailable) => None,
        Err(error) => {
            tracing::warn!(digest = digest.as_str(), %error, "remote placement read-through failed");
            None
        }
    }
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
    pub const fn required_acks(self, configured: usize) -> usize {
        match self {
            Self::Local => 1,
            Self::Majority => configured / 2 + 1,
            Self::Everywhere => configured,
            Self::AtLeast(acks) => acks.get(),
        }
    }
}

/// A peer transport failure exposed to availability implementations.
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
}

/// Proof that one peer durably holds an artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerReceipt {
    pub node: String,
    pub digest: Digest,
    pub size: u64,
}

/// Same-datacenter receipt lookup implemented by local or network transports.
#[async_trait]
pub trait ReceiptSource: Sync {
    fn node(&self) -> &str;
    async fn fetch_receipt(&self, digest: &Digest) -> Result<Option<PeerReceipt>, TransportError>;
}

/// A remote datacenter's accepted metadata frontier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAck {
    pub datacenter: String,
    pub epoch: u64,
    pub applied_frontier: u64,
}

/// Metadata frontier lookup implemented by local or network transports.
#[async_trait]
pub trait RemoteFrontierSource: Sync {
    fn datacenter(&self) -> &str;
    async fn fetch_frontier(&self, authority: &str) -> Result<Option<RemoteAck>, TransportError>;
}

/// The metadata position an HA write must prove remotely durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetadataOperation {
    pub epoch: u64,
    pub frontier: u64,
}

/// Evidence a hosted write asks the configured availability runtime to prove.
#[derive(Debug, Clone, Copy)]
pub struct WriteAckRequest<'a> {
    pub digest: &'a Digest,
    pub authority: &'a str,
    pub operation: MetadataOperation,
}

/// A byte-quorum decision and its collected node identities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ByteAckDecision {
    Acknowledged { nodes: Vec<String> },
    Pending { nodes: Vec<String>, remaining: usize },
}

impl ByteAckDecision {
    #[must_use]
    pub const fn is_acknowledged(&self) -> bool {
        matches!(self, Self::Acknowledged { .. })
    }
}

/// Byte durability evidence for a write's storage backend.
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

/// Whether a client write still has time to gather durability evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deadline {
    Live,
    Expired,
}

/// A write's datacenter durability result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DcAck {
    Durable { scope: BlobDurability },
    Pending,
    Unknown,
}

/// Resolves write durability without exposing deployment mechanics to an ecosystem.
#[async_trait]
pub trait WriteAcknowledger: Send + Sync {
    async fn acknowledge(&self, request: WriteAckRequest<'_>) -> DcAck;
}

/// Receives bounded write-durability measurements from an availability runtime.
pub trait WriteAckObserver: Send + Sync {
    fn record(&self, outcome: DcAck, byte_decision: &ByteAckDecision);
}

/// Log-safe availability operation fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OperationTelemetry {
    pub source: String,
    pub epoch: u64,
    pub serial: u64,
    pub kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub traceparent: Option<String>,
    pub sampled: bool,
}

impl OperationTelemetry {
    pub fn emit(&self) {
        let Some(traceparent) = self.traceparent.as_deref().filter(|_| self.sampled) else {
            return;
        };
        tracing::info!(
            operation.source = %self.source,
            operation.epoch = self.epoch,
            operation.serial = self.serial,
            operation.kind = self.kind,
            operation.traceparent = traceparent,
            "availability operation",
        );
    }
}

/// Whether a W3C trace context opts an operation into recording.
#[must_use]
pub fn sampled(traceparent: Option<&str>) -> bool {
    traceparent.and_then(trace_flags).is_some_and(|flags| flags & 0x01 != 0)
}

fn trace_flags(traceparent: &str) -> Option<u8> {
    let flags = traceparent
        .rfind('-')
        .map_or(traceparent, |dash| &traceparent[dash + 1..]);
    (flags.len() == 2).then(|| u8::from_str_radix(flags, 16).ok()).flatten()
}
