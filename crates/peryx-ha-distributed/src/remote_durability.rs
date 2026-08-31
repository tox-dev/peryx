pub use peryx_ha::{DurabilityPolicy, MetadataOperation, RemoteAck};
use std::cmp::Reverse;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteDurability {
    Durable {
        holders: Vec<String>,
    },
    /// `durable_frontier` is the serial the required datacenters have all applied through.
    Pending {
        holders: Vec<String>,
        durable_frontier: u64,
    },
}

impl RemoteDurability {
    #[must_use]
    pub const fn is_durable(&self) -> bool {
        matches!(self, Self::Durable { .. })
    }

    #[must_use]
    pub fn holders(&self) -> &[String] {
        match self {
            Self::Durable { holders } | Self::Pending { holders, .. } => holders,
        }
    }
}

/// Requires `policy` over `configured` remote datacenters to report the operation epoch and an applied
/// frontier at or beyond the operation frontier. Ignores other epochs and counts each datacenter once.
///
/// The quorum spans the remote datacenters alone, because the writer's own datacenter proves the write
/// through the byte dimension instead. The minimum quorum is one, so `Everywhere` over an empty remote
/// set cannot claim durability from no evidence. Pending results retain the covering datacenters and the
/// frontier the quorum has already applied through, so a caller can report real progress.
#[must_use]
pub fn assess_remote_metadata_durability(
    operation: &MetadataOperation,
    acks: &[RemoteAck],
    configured: usize,
    policy: DurabilityPolicy,
) -> RemoteDurability {
    let mut applied: BTreeMap<&str, u64> = BTreeMap::new();
    for ack in acks.iter().filter(|ack| ack.epoch == operation.epoch) {
        let slot = applied.entry(ack.datacenter.as_str()).or_default();
        *slot = (*slot).max(ack.applied_frontier);
    }
    let required = policy.required_acks(configured).max(1);
    let holders: Vec<String> = applied
        .iter()
        .filter(|(_, frontier)| **frontier >= operation.frontier)
        .map(|(datacenter, _)| (*datacenter).to_owned())
        .collect();
    if holders.len() >= required {
        RemoteDurability::Durable { holders }
    } else {
        RemoteDurability::Pending {
            durable_frontier: quorum_frontier(applied.into_values(), required),
            holders,
        }
    }
}

/// The `required`-th largest applied serial, or zero when fewer datacenters report the epoch.
fn quorum_frontier(reported: impl Iterator<Item = u64>, required: usize) -> u64 {
    let mut applied: Vec<u64> = reported.collect();
    applied.sort_unstable_by_key(|&serial| Reverse(serial));
    applied.get(required - 1).copied().unwrap_or_default()
}
