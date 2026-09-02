//! One W3C trace per availability write, opened where the write commits and recorded where its
//! acknowledgement decides.
//!
//! The verdict a client sees is a single word: the write is durable, or it is not. The evidence behind
//! that word — the datacenter members whose receipt was counted, the threshold the configured policy
//! asked for, which of the two dimensions was still outstanding when the budget ran out — is computed
//! for every write and then reduced to a counter. A trace keeps it, keyed by the operation an operator
//! is asking about.
//!
//! A trace carries identity and verdict only. Artifact bytes, credentials, and private paths stay out.

use std::time::Duration;

use crate::{AuthorityEpoch, ByteEvidence, DcAck, DurabilityPolicy, OperationKind};

/// The identity of one committed availability write, which every record of that write is joined on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationObservation {
    /// The node that accepted the write.
    pub source: String,
    /// The authority the write mutates, which owns the epoch and journal frontier it is measured against.
    pub authority: String,
    pub epoch: AuthorityEpoch,
    /// The journal serial the write committed at, absent when the mutation journaled nothing. It is the
    /// write's own commit receipt, never a later global head.
    pub serial: Option<u64>,
    pub kind: OperationKind,
}

/// A committed write's open [W3C trace](https://www.w3.org/TR/trace-context/).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationTrace {
    pub operation: OperationObservation,
    pub traceparent: String,
}

impl OperationTrace {
    /// Opens a root trace whose trace and span identifiers are drawn from the operating system's
    /// entropy, as the trace-context specification requires. A random draw also stays unique when the
    /// same serial is replayed under a new epoch, which a hash of the operation's identity would not,
    /// and does not move when the standard library changes its default hasher.
    ///
    /// Version 4 UUIDs pin their version and variant bits, so neither identifier can come out all-zero.
    ///
    /// The sampled flag is always set. peryx opens a trace for a mutation only, a rate far below the
    /// read path a collector would otherwise have to sample down, so every write is worth keeping.
    #[must_use]
    pub fn open(operation: OperationObservation) -> Self {
        let trace_id = uuid::Uuid::new_v4().as_u128();
        let (span_id, _) = uuid::Uuid::new_v4().as_u64_pair();
        Self {
            operation,
            traceparent: format!("00-{trace_id:032x}-{span_id:016x}-01"),
        }
    }
}

/// What a blob write's acknowledgement proved, recorded against the write's open trace.
///
/// Both dimensions are reported separately. A blob write is datacenter-durable only once its bytes and
/// its metadata are, and the combined verdict alone does not say which of the two the write is waiting
/// on — the question an operator asks first when a write answers `503`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobAckObservation<'evidence> {
    pub policy: DurabilityPolicy,
    pub outcome: DcAck,
    pub bytes: &'evidence ByteEvidence,
    /// Whether the metadata dimension reached the write's journal serial.
    pub metadata_acknowledged: bool,
    /// Whether the byte dimension's budget expired before it proved.
    pub bytes_expired: bool,
    /// Whether the metadata dimension's budget expired before it proved.
    pub metadata_expired: bool,
    pub waited: Duration,
}

#[cfg(test)]
#[path = "../tests/unit/operation_trace/tests.rs"]
mod tests;
