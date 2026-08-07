//! Deciding whether one artifact's bytes are datacenter-acknowledged from the durability evidence.
//!
//! [`assess_byte_durability`](crate::assess_byte_durability) folds independent per-node receipts into
//! whether an artifact's bytes are datacenter-durable; [`decide_byte_ack`] turns that into a client
//! write's acknowledgement - acknowledged, or pending with how many more independent receipts it still
//! needs. It is the artifact-bytes counterpart to [`acknowledge`](crate::acknowledge), which decides a
//! metadata write from its journal frontiers.
//!
//! The decision is pure: the receipts are an input, so no transport, deadline, or storage is consulted.
//! The caller that gathers receipts under a deadline, and fails the write when the deadline expires
//! before quorum, composes this decision on top.

use std::collections::BTreeSet;

pub use peryx_ha::ByteAckDecision;
use peryx_storage::blob::Digest;

use crate::readiness::DurabilityPolicy;
use crate::receipt_quorum::{ByteDurability, ReceiptAck, assess_byte_durability};

/// Decide whether `digest`'s bytes are datacenter-acknowledged given the `acks` received so far, the
/// group's `members`, and its [`DurabilityPolicy`].
///
/// Folds the evidence through [`assess_byte_durability`] and reports the write's acknowledgement: a
/// durable result acknowledges, and a pending one carries how many more independent receipts remain, so
/// a caller waiting on a deadline knows exactly what it is still waiting for.
#[must_use]
pub fn decide_byte_ack(
    digest: &Digest,
    acks: &[ReceiptAck],
    members: &BTreeSet<String>,
    policy: DurabilityPolicy,
) -> ByteAckDecision {
    match assess_byte_durability(digest, acks, members, policy) {
        ByteDurability::Durable { nodes } => ByteAckDecision::Acknowledged { nodes },
        ByteDurability::Pending { nodes, required } => ByteAckDecision::Pending {
            remaining: required - nodes.len(),
            nodes,
        },
    }
}
