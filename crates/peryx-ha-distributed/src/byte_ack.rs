use std::collections::BTreeSet;

pub use peryx_ha::ByteAckDecision;
use peryx_storage::blob::Digest;

use crate::readiness::DurabilityPolicy;
use crate::receipt_quorum::{ByteDurability, ReceiptAck, assess_byte_durability};

/// Pending decisions report the number of additional independent receipts required.
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
