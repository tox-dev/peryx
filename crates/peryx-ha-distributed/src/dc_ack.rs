//! Deciding a client write's datacenter acknowledgement from the metadata and artifact evidence.
//!
//! In `dc` mode an HTTP write returns success only once the selected backend proves durability for both
//! dimensions: the metadata write and the artifact bytes. [`acknowledge`](crate::acknowledge) decides
//! the metadata dimension from a group's journal frontiers; [`decide_byte_ack`](crate::decide_byte_ack)
//! decides a filesystem backend's byte dimension from its receipt quorum. This is the shared decision
//! that combines a dimension from each side under the write's deadline, so one place turns the evidence
//! into the client outcome. It reads the two decisions and the deadline and consults no transport,
//! clock, or storage.
//!
//! The byte evidence carries its own scope: a filesystem backend proves datacenter durability only from
//! a receipt quorum, while a DC-durable object store proves it from its own atomic-put-and-digest
//! acknowledgement. A deadline that expires before both dimensions prove is not a definite failure,
//! because the durable write may have completed after the client stopped waiting, so the outcome is
//! [`DcAck::Unknown`] rather than a false negative a retry would double-apply.

pub use peryx_ha::{ByteEvidence, DcAck, Deadline};

use crate::ack::AckDecision;
/// Decide a client write's datacenter acknowledgement from its `metadata` and byte `evidence` under the
/// write's `deadline`.
///
/// The write is [`Durable`](DcAck::Durable) only when both the metadata dimension and the backend's byte
/// dimension are acknowledged, and it carries the scope the byte evidence fixes. While either dimension
/// is unproven and the deadline is live the caller keeps waiting ([`Pending`](DcAck::Pending)); once the
/// deadline expires with a dimension unproven the outcome is [`Unknown`](DcAck::Unknown) rather than a
/// failure, because the durable write may have completed after the client stopped waiting.
#[must_use]
pub const fn decide_dc_ack(metadata: AckDecision, evidence: &ByteEvidence, deadline: Deadline) -> DcAck {
    if metadata.is_acknowledged() && evidence.is_durable() {
        DcAck::Durable {
            scope: evidence.scope(),
        }
    } else {
        match deadline {
            Deadline::Live => DcAck::Pending,
            Deadline::Expired => DcAck::Unknown,
        }
    }
}
