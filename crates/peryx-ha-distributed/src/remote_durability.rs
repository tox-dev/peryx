pub use peryx_ha::{MetadataOperation, RemoteAck};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteDurability {
    Durable { holders: Vec<String> },
    Pending,
}

impl RemoteDurability {
    #[must_use]
    pub const fn is_durable(&self) -> bool {
        matches!(self, Self::Durable { .. })
    }

    #[must_use]
    pub fn holders(&self) -> &[String] {
        match self {
            Self::Durable { holders } => holders,
            Self::Pending => &[],
        }
    }

    /// Removing the sole holder would eliminate remote durability.
    #[must_use]
    pub fn is_sole_copy(&self) -> bool {
        self.holders().len() == 1
    }
}

/// Requires one datacenter to report the operation epoch and an applied frontier at or beyond the
/// operation frontier. Ignores other epochs and counts each datacenter once.
#[must_use]
pub fn assess_remote_metadata_durability(operation: &MetadataOperation, acks: &[RemoteAck]) -> RemoteDurability {
    let holders: Vec<String> = acks
        .iter()
        .filter(|ack| {
            let epoch_matches = ack.epoch == operation.epoch;
            let frontier_covered = ack.applied_frontier >= operation.frontier;
            epoch_matches && frontier_covered
        })
        .map(|ack| ack.datacenter.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if holders.is_empty() {
        RemoteDurability::Pending
    } else {
        RemoteDurability::Durable { holders }
    }
}
