//! Acknowledging one artifact's bytes as datacenter-durable from independent per-node receipts.
//!
//! [`acknowledge`](crate::acknowledge) proves a metadata write durable from a group's journal
//! frontiers; this is the separate dimension its doc defers, the artifact bytes. A filesystem backend
//! proves only single-node durability, so a datacenter guarantees an artifact only once enough
//! independent nodes each return a verified placement receipt for it. The fold is pure: the receipts
//! are an input, deduplicated by node so two paths on one node never count twice and filtered to the
//! digest under write so a stray or misrouted receipt never counts, then measured against the group's
//! [`DurabilityPolicy`].

use std::collections::BTreeSet;

use peryx_storage::blob::Digest;

use crate::readiness::DurabilityPolicy;

/// One node's proof that it durably holds an artifact's bytes: the node that returned a placement
/// receipt and the digest that receipt verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptAck {
    /// The independent configured node the receipt came from.
    pub node: String,
    /// The digest the node's receipt proved durable.
    pub digest: Digest,
}

/// Whether an artifact's bytes are datacenter-durable, carrying the independent nodes that proved it so
/// a caller can preserve the evidence for a safe retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ByteDurability {
    /// The policy's required number of independent nodes returned a matching receipt.
    Durable { nodes: Vec<String> },
    /// Fewer independent nodes than the policy requires have proven durability so far.
    Pending { nodes: Vec<String>, required: usize },
}

impl ByteDurability {
    /// The independent nodes that have proven durability, whichever the outcome.
    #[must_use]
    pub fn nodes(&self) -> &[String] {
        match self {
            Self::Durable { nodes } | Self::Pending { nodes, .. } => nodes,
        }
    }

    /// Whether the bytes reached datacenter durability.
    #[must_use]
    pub const fn is_durable(&self) -> bool {
        matches!(self, Self::Durable { .. })
    }
}

/// Decide whether `digest`'s bytes are datacenter-durable given the `acks` received so far, the group's
/// `members`, and its [`DurabilityPolicy`].
///
/// A receipt counts only when it is for `digest` and comes from a configured member, so a receipt for
/// another digest, a stray one from a node outside the group, or a second path on a node already counted
/// never inflates the tally. The quorum required is [`DurabilityPolicy::required_acks`] over the member
/// count, floored at one: a datacenter never proves an artifact durable without at least one independent
/// node holding it, which is what `Everywhere` over an empty roster would otherwise claim vacuously. The
/// bytes are durable once the distinct member count reaches that quorum; below it the result is pending
/// and carries the acknowledging nodes, so a retry resumes from the copies already made.
#[must_use]
pub fn assess_byte_durability(
    digest: &Digest,
    acks: &[ReceiptAck],
    members: &BTreeSet<String>,
    policy: DurabilityPolicy,
) -> ByteDurability {
    let nodes: Vec<String> = acks
        .iter()
        .filter(|ack| &ack.digest == digest && members.contains(&ack.node))
        .map(|ack| ack.node.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let required = policy.required_acks(members.len()).max(1);
    if nodes.len() >= required {
        ByteDurability::Durable { nodes }
    } else {
        ByteDurability::Pending { nodes, required }
    }
}
