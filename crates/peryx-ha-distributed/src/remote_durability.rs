//! Deciding whether a metadata write is remote-durable in HA mode from per-DC acknowledgement
//! frontiers.
//!
//! HA success requires one eligible remote datacenter to commit the exact metadata operation, not a
//! majority and not every DC. Each remote reports how far it has durably applied under which authority
//! epoch; [`assess_remote_metadata_durability`] folds those acknowledgements against the operation's
//! epoch and apply frontier and reports whether an eligible remote holds it, naming the datacenters that
//! do so the caller can warn before removing the sole remote copy.
//!
//! The fold is pure: the acknowledgements are an input, so no transport, deadline, or storage is
//! consulted. A remote is eligible only at the operation's own epoch - a stale or advanced epoch is
//! fenced out - and only once it has applied through the operation's frontier. Selecting which replicas
//! to wait on, enforcing the deadline, and the retry lookup compose on top.

pub use peryx_ha::{MetadataOperation, RemoteAck};
use std::collections::BTreeSet;

/// Whether a metadata write is remote-durable, naming the datacenters that hold it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteDurability {
    /// At least one eligible remote committed the operation; these datacenters hold a durable copy.
    Durable { holders: Vec<String> },
    /// No eligible remote has committed the operation yet.
    Pending,
}

impl RemoteDurability {
    /// Whether an eligible remote holds the operation.
    #[must_use]
    pub const fn is_durable(&self) -> bool {
        matches!(self, Self::Durable { .. })
    }

    /// The datacenters holding a durable copy, empty while pending.
    #[must_use]
    pub fn holders(&self) -> &[String] {
        match self {
            Self::Durable { holders } => holders,
            Self::Pending => &[],
        }
    }

    /// Whether exactly one remote holds the operation, so removing it would drop the last remote copy.
    #[must_use]
    pub fn is_sole_copy(&self) -> bool {
        self.holders().len() == 1
    }
}

/// Decide whether `operation` is remote-durable given the acknowledgements received so far.
///
/// A remote counts only when its epoch matches the operation and it has applied through the operation's
/// frontier. Stale and advanced epochs are fenced. Each datacenter counts once however many
/// acknowledgements it sent, so a duplicated or reordered response never inflates the holder set. One
/// eligible remote is enough; below that the write is pending.
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
