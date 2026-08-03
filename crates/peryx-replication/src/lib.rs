//! Primary/replica replication over peryx's ordered storage journal.
//!
//! A primary exposes [`ChangePage`] records and digest-addressed blob streams through [`Primary`].
//! [`Replica`] verifies the serial sequence and every missing blob before committing metadata,
//! copied journal entries, and its resume cursor in one transaction.

mod analytics;
mod consensus;
mod election;
mod envelope;
mod error;
mod follower;
mod http;
mod liveness;
mod peer;
mod peer_http;
mod protocol;
mod replica;
pub mod sim;

pub use analytics::{
    APPLY_STATE_SCHEMA, AggregateDelta, AggregateKey, AggregateRow, AnalyticsBatch, ApplyError, ApplyLimits,
    ApplyOutcome, ApplyState, DEFAULT_APPLY_LIMITS, Frontier, IntervalId, ProducerId, SnapshotError,
};
pub use consensus::{
    AppendEntries, AppendOutcome, DEFAULT_LOG_LIMITS, LogEntry, LogIndex, LogLimits, MemoryRaftLog, RaftLog,
    RaftLogError, Term,
};
pub use election::{ElectionError, NodeId, PersistentState, VoteDecision, VoteReason, VoteRequest};
pub use envelope::{
    AuthorityEpoch, DEFAULT_DECODE_LIMITS, DecodeLimits, EnvelopeError, OperationEnvelope, OperationId, OperationKind,
    SCHEMA_VERSION, SchemaVersion, TraceContext,
};
pub use error::SyncError;
pub use follower::{
    AppendAccepted, AppendReject, AppendRequest, AppendResponse, CommitTracker, receive_append_entries,
};
pub use http::{DEFAULT_MAX_CHANGE_PAGE_SIZE, HttpPrimary, HttpPrimaryError, PrimaryHttpConfigError, primary_router};
pub use liveness::{
    DEFAULT_DEAD_AFTER, DEFAULT_MAX_HEARTBEAT_BYTES, DEFAULT_SUSPECT_AFTER, HeartbeatReport, LivenessRejection,
    LivenessTracker, PeerHealth, Suspicion, liveness_router,
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
pub use replica::{Replica, ReplicaState, SyncOutcome};

#[cfg(test)]
mod analytics_tests;
#[cfg(test)]
mod consensus_tests;
#[cfg(test)]
mod election_tests;
#[cfg(test)]
mod envelope_tests;
#[cfg(test)]
mod follower_tests;
#[cfg(test)]
mod http_tests;
#[cfg(test)]
mod liveness_tests;
#[cfg(test)]
mod peer_http_tests;
#[cfg(test)]
mod peer_tests;
#[cfg(test)]
mod protocol_tests;
#[cfg(test)]
mod tests;
