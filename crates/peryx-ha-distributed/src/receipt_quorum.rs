use std::collections::BTreeSet;

use peryx_storage::blob::Digest;

use crate::readiness::DurabilityPolicy;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptAck {
    pub node: String,
    pub digest: Digest,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ByteDurability {
    Durable { nodes: Vec<String> },
    Pending { nodes: Vec<String>, required: usize },
}

impl ByteDurability {
    #[must_use]
    pub fn nodes(&self) -> &[String] {
        match self {
            Self::Durable { nodes } | Self::Pending { nodes, .. } => nodes,
        }
    }

    #[must_use]
    pub const fn is_durable(&self) -> bool {
        matches!(self, Self::Durable { .. })
    }
}

/// Counts each configured node once when its receipt matches `digest`.
///
/// The minimum quorum is one, preventing `Everywhere` over an empty member set from claiming durability.
/// Pending results retain counted nodes so a retry can reuse existing copies.
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
