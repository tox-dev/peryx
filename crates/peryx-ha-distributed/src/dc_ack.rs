//! A write is datacenter-durable after both its metadata and bytes are durable.
//!
//! An expired deadline with incomplete evidence yields [`DcAck::Unknown`]. The write may commit after the
//! client stops waiting, so reporting failure could cause a retry to apply it twice.

pub use peryx_ha::{ByteEvidence, DcAck, Deadline};

use crate::ack::AckDecision;
/// Requires both dimensions for [`DcAck::Durable`]. Incomplete evidence yields [`DcAck::Pending`] before
/// the deadline and [`DcAck::Unknown`] after it expires.
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
