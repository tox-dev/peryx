//! Primary/replica replication over peryx's ordered storage journal.
//!
//! A primary exposes [`ChangePage`] records and digest-addressed blob streams through [`Primary`].
//! [`Replica`] verifies the serial sequence and every missing blob before committing metadata,
//! copied journal entries, and its resume cursor in one transaction.

mod envelope;
mod error;
mod http;
mod protocol;
mod replica;

pub use envelope::{
    AuthorityEpoch, CURRENT_SCHEMA_VERSION, DEFAULT_DECODE_LIMITS, DecodeLimits, EnvelopeError, MIN_SCHEMA_VERSION,
    OperationEnvelope, OperationId, OperationKind, SUPPORTED_SCHEMA_VERSIONS, SchemaVersion, TraceContext,
};
pub use error::SyncError;
pub use http::{DEFAULT_MAX_CHANGE_PAGE_SIZE, HttpPrimary, HttpPrimaryError, PrimaryHttpConfigError, primary_router};
pub use protocol::{BlobReference, Change, ChangePage, MetadataMutation, PROTOCOL_VERSION, Primary};
pub use replica::{Replica, ReplicaState, SyncOutcome};

#[cfg(test)]
mod envelope_tests;
#[cfg(test)]
mod http_tests;
#[cfg(test)]
mod tests;
