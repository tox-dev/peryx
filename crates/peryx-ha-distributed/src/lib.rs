//! Primary/replica replication over peryx's ordered storage journal.
//!
//! A primary exposes [`ChangePage`] records and digest-addressed blob streams through [`Primary`].
//! [`Replica`] verifies the serial sequence and every missing blob before committing metadata,
//! copied journal entries, and its resume cursor in one transaction.

mod ack;
mod analytics;
mod analytics_transfer;
mod authority;
mod backoff;
mod beacon;
mod blob;
mod blob_availability;
mod blob_fetch;
mod blob_http;
mod blob_piece;
mod blob_placement;
mod blob_plane;
mod blob_pull;
mod blob_reassembly;
mod blob_routing;
mod byte_ack;
mod channel;
mod circuit;
mod completeness;
mod completeness_query;
mod dc_ack;
mod dc_copy;
mod drain;
mod driver;
mod envelope;
mod error;
mod failover;
mod filesystem_ack;
mod http;
mod ingress_intent;
mod liveness;
mod multi_peer;
mod multi_pull;
mod ownership;
mod peer;
mod peer_http;
mod peer_receipt;
mod peer_receipt_http;
mod protocol;
pub mod raft;
pub mod read_through;
mod readiness;
mod receipt_quorum;
mod reconcile;
mod remote_durability;
mod remote_frontier;
mod remote_frontier_http;
mod replica;
mod rollout;
pub mod sim;
mod status;
mod telemetry;
mod transfer;
mod upgrade;
mod versions;
mod visibility;
mod visibility_feed;
mod visibility_mint;

pub use ack::{AckDecision, acknowledge};
pub use analytics::{
    APPLY_STATE_SCHEMA, AggregateDelta, AggregateKey, AggregateRow, AnalyticsBatch, AnalyticsReceiver, ApplyError,
    ApplyLimits, ApplyOutcome, ApplyState, DEFAULT_APPLY_LIMITS, Frontier, IntervalId, ProducerId, SnapshotError,
};
pub use analytics_transfer::{
    AnalyticsPullError, AnalyticsSource, HttpAnalyticsError, HttpAnalyticsSource, PullReport, pull,
};
pub use authority::{Admission, AuthorityFence, AuthorityKey, CommitOutcome};
pub use backoff::{DEFAULT_RECONNECT_POLICY, RETRY_EXHAUSTED, ReconnectPolicy, Retry};
pub use beacon::{BeaconError, BeaconSender, DEFAULT_BEACON_INTERVAL};
pub use blob::{BlobRequest, BlobTransport, ByteRange, CapacityLimited, LoopbackBlobSource};
pub use blob_availability::{BlobAvailability, ReferencedBlob, blob_availability};
pub use blob_fetch::{FetchOutcome, FetchReport, fetch_missing};
pub use blob_http::{HttpBlobError, HttpBlobTransport};
pub use blob_piece::{PieceError, blob_piece};
pub use blob_placement::{FetchPlan, plan_blob_fetch};
pub use blob_plane::{
    BLOB_VIEW, BlobPlaneReport, BlobSources, advance_blob_frontier, pull_outstanding, pull_referenced,
};
pub use blob_pull::{
    ChunkFailure, ChunkUnavailable, PullError, chunk_ranges, pull_chunk_verified, pull_ranged, pull_ranged_blob,
};
pub use blob_reassembly::{BlobPiece, ReassemblyError, reassemble_verified};
pub use blob_routing::RoutingBlobTransport;
pub use byte_ack::{ByteAckDecision, decide_byte_ack};
pub use channel::{BoundedChannel, BufferOutcome, ChannelFull, buffer_batch};
pub use circuit::{CircuitBreaker, CircuitConfig, DEFAULT_CIRCUIT};
pub use completeness::{ProducerCoverage, assess};
pub use completeness_query::{DistributedAnalyticsCompleteness, assess_completeness};
pub use dc_ack::{ByteEvidence, DcAck, Deadline, decide_dc_ack};
pub use dc_copy::{CopyError, copy_blob_to_target};
pub use drain::{DrainIntent, DrainPlan, plan_drain};
pub use driver::{StepOutcome, advance_once};
pub use envelope::{
    AuthorityEpoch, DEFAULT_DECODE_LIMITS, DecodeLimits, EnvelopeError, OperationEnvelope, OperationId, OperationKind,
    SCHEMA_VERSION, SchemaVersion, TraceContext, TraceError, derive_child,
};
pub use error::SyncError;
pub use failover::{Candidate, Failover, FailoverPolicy};
pub use filesystem_ack::{FilesystemAck, ReceiptOutcome};
pub use http::{
    DEFAULT_MAX_CHANGE_PAGE_BYTES, DEFAULT_MAX_CHANGE_PAGE_SIZE, DEFAULT_MAX_CONCURRENT_BLOB_STREAMS, HttpPrimary,
    HttpPrimaryError, PrimaryHttpConfigError, follower_router, primary_router, primary_router_with_stream_limit,
};
pub use ingress_intent::{IngressIntent, IntentKey, IntentLedger, IntentState, StageOutcome, TransitionOutcome};
pub use liveness::{
    DEFAULT_DEAD_AFTER, DEFAULT_MAX_HEARTBEAT_BYTES, DEFAULT_SUSPECT_AFTER, HeartbeatReport, LivenessRejection,
    LivenessTracker, PeerHealth, Suspicion, liveness_router,
};
pub use multi_peer::{DEFAULT_SET_LIMITS, MemberOutcome, PeerSet, RoundReport, SetLimits};
pub use multi_pull::{PullRound, pull_round};
pub use ownership::{
    AppliedMeta, Assignment, AssignmentCause, DatacenterId, OwnershipCommand, OwnershipEffect, OwnershipError,
    OwnershipState, Rejection, TransferRecord,
};
pub use peer::{
    BatchFrame, BatchRequest, DEFAULT_TRANSFER_LIMITS, FrontierSync, LoopbackPeer, LoopbackTransport, PeerFault,
    PeerTransport, TransferLimits, TransportError, drain_to_frontier,
};
pub use peer_http::{HttpPeerError, HttpPeerTransport};
pub use peer_receipt::{DEFAULT_RECEIPT_POLL, LoopbackReceiptSource, PeerReceipt, ReceiptSource, gather_receipts};
pub use peer_receipt_http::{HttpReceiptError, HttpReceiptSource, ReceiptReply, receipt_router};
pub use peryx_ha::{Completeness, CompletenessQuery, CompletenessReport, DayBucket, ExpectedProducer, ProducerReport};
pub use protocol::{
    BlobReference, Change, ChangePage, MetadataMutation, PROTOCOL_VERSION, PlacementAvailability, PlacementDescriptor,
    Primary,
};
pub use readiness::{
    DurabilityPolicy, GroupReadiness, MemberFrontier, MemberRole, ReadinessBlocker, group_readiness,
    visibility_compaction_frontier,
};
pub use receipt_quorum::{ByteDurability, ReceiptAck, assess_byte_durability};
pub use reconcile::{
    Cleanup, Disposition, OldEpochIdentity, OldEpochOp, ReconcileAction, ReconcileDrain, ReplayCommand, classify,
    cleanup, drain_reconcile, reconcile,
};
pub use remote_durability::{MetadataOperation, RemoteAck, RemoteDurability, assess_remote_metadata_durability};
pub use remote_frontier::{
    DEFAULT_FRONTIER_POLL, LoopbackRemoteFrontierSource, RemoteFrontierSource, gather_remote_acks,
};
pub use remote_frontier_http::{
    FrontierReply, HttpRemoteFrontierError, HttpRemoteFrontierSource, MetadataFrontierProvider, frontier_router,
};
pub use replica::{AppliedPage, Replica, ReplicaState, SyncOutcome};
pub use rollout::{RolloutBlocker, RolloutBudget, RolloutPreflight, rollout_preflight, upgrade_order};
pub use status::{OperationStatus, WriteRecord};
pub use telemetry::{OperationTelemetry, operation_telemetry, sampled};
pub use transfer::{TransferAudit, TransferError, TransferPhase, TransferPlan, TransferRequest};
pub use upgrade::{Preflight, PreflightBlocker, UpgradeTarget, upgrade_preflight};
pub use versions::{
    AvailabilityVersions, Incompatibility, Negotiation, Version, VersionRange, WireKind, accepts_operation_kind,
    feature_activated, negotiate, snapshot_compatible,
};
pub use visibility::{
    ApplyEffect, ArtifactId, Frontier as VisibilityFrontier, OpOrder, SnapshotError as VisibilitySnapshotError,
    VISIBILITY_APPLY_SCHEMA, Visibility, VisibilityAction, VisibilityOp, VisibilityState,
};
pub use visibility_feed::{
    ApplyEnvelopeError, OpenError, VISIBILITY_CHANGE_SCHEMA, VisibilityFeedError, VisibilityProjection,
    VisibilitySnapshotStore, decode_visibility_op, visibility_change, visibility_envelope,
};
pub use visibility_mint::{JournalSerials, SerialSource, StaleEpoch, VisibilityMinter};

#[cfg(test)]
mod ack_tests;
#[cfg(test)]
mod analytics_tests;
#[cfg(test)]
mod authority_tests;
#[cfg(test)]
mod backoff_tests;
#[cfg(test)]
mod beacon_tests;
#[cfg(test)]
mod blob_availability_tests;
#[cfg(test)]
mod blob_fetch_tests;
#[cfg(test)]
mod blob_http_tests;
#[cfg(test)]
mod blob_piece_tests;
#[cfg(test)]
mod blob_placement_tests;
#[cfg(test)]
mod blob_plane_tests;
#[cfg(test)]
mod blob_pull_tests;
#[cfg(test)]
mod blob_reassembly_tests;
#[cfg(test)]
mod blob_routing_tests;
#[cfg(test)]
mod blob_tests;
#[cfg(test)]
mod byte_ack_tests;
#[cfg(test)]
mod channel_tests;
#[cfg(test)]
mod circuit_tests;
#[cfg(test)]
mod completeness_query_tests;
#[cfg(test)]
mod completeness_tests;
#[cfg(test)]
mod dc_ack_tests;
#[cfg(test)]
mod dc_copy_tests;
#[cfg(test)]
mod drain_tests;
#[cfg(test)]
mod driver_tests;
#[cfg(test)]
mod envelope_tests;
#[cfg(test)]
mod epoch_reservation_tests;
#[cfg(test)]
mod failover_tests;
#[cfg(test)]
mod filesystem_ack_tests;
#[cfg(test)]
mod http_tests;
#[cfg(test)]
mod ingress_intent_tests;
#[cfg(test)]
mod liveness_tests;
#[cfg(test)]
mod multi_peer_tests;
#[cfg(test)]
mod ownership_tests;
#[cfg(test)]
mod peer_http_tests;
#[cfg(test)]
mod peer_receipt_http_tests;
#[cfg(test)]
mod peer_receipt_tests;
#[cfg(test)]
mod peer_tests;
#[cfg(test)]
mod protocol_tests;
#[cfg(test)]
mod readiness_tests;
#[cfg(test)]
mod receipt_quorum_tests;
#[cfg(test)]
mod reconcile_tests;
#[cfg(test)]
mod remote_durability_tests;
#[cfg(test)]
mod remote_frontier_http_tests;
#[cfg(test)]
mod remote_frontier_tests;
#[cfg(test)]
mod rollout_tests;
#[cfg(test)]
mod status_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod transfer_tests;
#[cfg(test)]
mod upgrade_tests;
#[cfg(test)]
mod versions_tests;
#[cfg(test)]
mod visibility_feed_tests;
#[cfg(test)]
mod visibility_mint_tests;
#[cfg(test)]
mod visibility_tests;
