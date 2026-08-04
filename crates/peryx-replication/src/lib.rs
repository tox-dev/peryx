//! Primary/replica replication over peryx's ordered storage journal.
//!
//! A primary exposes [`ChangePage`] records and digest-addressed blob streams through [`Primary`].
//! [`Replica`] verifies the serial sequence and every missing blob before committing metadata,
//! copied journal entries, and its resume cursor in one transaction.

mod ack;
mod analytics;
mod authority;
mod backoff;
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
mod completeness;
mod dc_copy;
mod driver;
mod envelope;
mod error;
mod http;
mod ingress_intent;
mod liveness;
mod ownership;
mod peer;
mod peer_http;
mod protocol;
pub mod raft;
mod readiness;
mod receipt_quorum;
mod reconcile;
mod remote_durability;
mod replica;
pub mod sim;
mod status;
mod versions;
mod visibility;

pub use ack::{AckDecision, acknowledge};
pub use analytics::{
    APPLY_STATE_SCHEMA, AggregateDelta, AggregateKey, AggregateRow, AnalyticsBatch, ApplyError, ApplyLimits,
    ApplyOutcome, ApplyState, DEFAULT_APPLY_LIMITS, Frontier, IntervalId, ProducerId, SnapshotError,
};
pub use authority::{Admission, AuthorityFence, AuthorityKey, CommitOutcome};
pub use backoff::{DEFAULT_RECONNECT_POLICY, RETRY_EXHAUSTED, ReconnectPolicy, Retry};
pub use blob::{BlobRequest, BlobTransport, ByteRange, CapacityLimited, LoopbackBlobSource};
pub use blob_availability::{BlobAvailability, ReferencedBlob, blob_availability};
pub use blob_fetch::{FetchOutcome, FetchReport, fetch_missing};
pub use blob_http::{HttpBlobError, HttpBlobTransport};
pub use blob_piece::{PieceError, blob_piece};
pub use blob_placement::{FetchPlan, plan_blob_fetch};
pub use blob_plane::{
    BLOB_VIEW, BlobPlaneReport, BlobSources, advance_blob_frontier, pull_outstanding, pull_referenced,
};
pub use blob_pull::{PullError, pull_ranged, pull_ranged_blob};
pub use blob_reassembly::{BlobPiece, ReassemblyError, reassemble_verified};
pub use blob_routing::RoutingBlobTransport;
pub use byte_ack::{ByteAckDecision, decide_byte_ack};
pub use channel::{BoundedChannel, BufferOutcome, ChannelFull, buffer_batch};
pub use completeness::{Completeness, ProducerCoverage, assess};
pub use dc_copy::{CopyPlan, plan_dc_copy};
pub use driver::{StepOutcome, advance_once};
pub use envelope::{
    AuthorityEpoch, DEFAULT_DECODE_LIMITS, DecodeLimits, EnvelopeError, OperationEnvelope, OperationId, OperationKind,
    SCHEMA_VERSION, SchemaVersion, TraceContext, TraceError, derive_child,
};
pub use error::SyncError;
pub use http::{DEFAULT_MAX_CHANGE_PAGE_SIZE, HttpPrimary, HttpPrimaryError, PrimaryHttpConfigError, primary_router};
pub use ingress_intent::{
    Ecosystem, IngressIntent, IntentKey, IntentLedger, IntentState, StageOutcome, TransitionOutcome,
};
pub use liveness::{
    DEFAULT_DEAD_AFTER, DEFAULT_MAX_HEARTBEAT_BYTES, DEFAULT_SUSPECT_AFTER, HeartbeatReport, LivenessRejection,
    LivenessTracker, PeerHealth, Suspicion, liveness_router,
};
pub use ownership::{
    DatacenterId, OwnershipCommand, OwnershipEffect, OwnershipError, OwnershipState, Rejection, TransferRecord,
};
pub use peer::{
    BatchFrame, BatchRequest, DEFAULT_TRANSFER_LIMITS, FrontierSync, LoopbackPeer, LoopbackTransport, PeerFault,
    PeerTransport, TransferLimits, TransportError, drain_to_frontier,
};
pub use peer_http::{HttpPeerError, HttpPeerTransport};
pub use protocol::{
    BlobReference, Change, ChangePage, MetadataMutation, PROTOCOL_VERSION, PlacementAvailability, PlacementDescriptor,
    Primary,
};
pub use readiness::{
    DurabilityPolicy, GroupReadiness, MemberFrontier, MemberRole, ReadinessBlocker, group_readiness,
    visibility_compaction_frontier,
};
pub use receipt_quorum::{ByteDurability, ReceiptAck, assess_byte_durability};
pub use reconcile::{Disposition, OldEpochOp, classify};
pub use remote_durability::{MetadataOperation, RemoteAck, RemoteDurability, assess_remote_metadata_durability};
pub use replica::{Replica, ReplicaState, SyncOutcome};
pub use status::{OperationStatus, WriteRecord};
pub use versions::{
    AvailabilityVersions, Incompatibility, Negotiation, Version, VersionRange, WireKind, accepts_operation_kind,
    feature_activated, negotiate, snapshot_compatible,
};
pub use visibility::{
    ApplyEffect, ArtifactId, Frontier as VisibilityFrontier, OpOrder, SnapshotError as VisibilitySnapshotError,
    VISIBILITY_APPLY_SCHEMA, Visibility, VisibilityAction, VisibilityOp, VisibilityState,
};

#[cfg(test)]
mod ack_tests;
#[cfg(test)]
mod analytics_tests;
#[cfg(test)]
mod authority_tests;
#[cfg(test)]
mod backoff_tests;
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
mod completeness_tests;
#[cfg(test)]
mod dc_copy_tests;
#[cfg(test)]
mod driver_tests;
#[cfg(test)]
mod envelope_tests;
#[cfg(test)]
mod http_tests;
#[cfg(test)]
mod ingress_intent_tests;
#[cfg(test)]
mod liveness_tests;
#[cfg(test)]
mod ownership_tests;
#[cfg(test)]
mod peer_http_tests;
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
mod status_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod versions_tests;
#[cfg(test)]
mod visibility_tests;
