//! Peer health remains unknown until consensus or an authoritative replica beacon reports it.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TopologyMode {
    /// Local durability with operator-driven failover.
    #[default]
    None,
    /// Explicit replicas within one datacenter.
    Dc,
    /// Remote-datacenter metadata durability.
    Ha,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeRole {
    /// Accepts authoritative writes.
    Writer,
    /// Applies writer changes and serves reads.
    Replica,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeLiveness {
    /// Serves reads at its frontier.
    Live,
    /// Running, but unable to serve from local storage.
    Unready,
    /// Must not be treated as healthy.
    Unknown,
}

/// Public callers see roster identity and roles, operators see live state, and administrators see addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopologyView {
    Public,
    Operator,
    Administrator,
}

impl TopologyView {
    const fn rank(self) -> u8 {
        match self {
            Self::Public => 0,
            Self::Operator => 1,
            Self::Administrator => 2,
        }
    }

    #[must_use]
    pub const fn admits(self, least: Self) -> bool {
        self.rank() >= least.rank()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopologyMember {
    pub node: String,
    pub dc: String,
    /// Visible only to administrators.
    pub address: String,
    pub role: NodeRole,
}

/// Local observations are never [`NodeLiveness::Unknown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalStatus {
    pub role: NodeRole,
    pub liveness: NodeLiveness,
    pub frontier: u64,
}

/// Live state is read when a snapshot is taken, not stored in this configuration.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TopologyConfig {
    pub mode: TopologyMode,
    pub group: Option<String>,
    pub members: Vec<TopologyMember>,
    /// Replicas leave this unset because they do not carry roster identity.
    pub local_node: Option<String>,
}

impl TopologyConfig {
    /// Returns `None` when no roster member identifies this process.
    #[must_use]
    pub fn local_datacenter(&self) -> Option<&str> {
        let local = self.local_node.as_deref()?;
        self.members
            .iter()
            .find(|member| member.node == local)
            .map(|member| member.dc.as_str())
    }
}

/// Snapshots cap their roster but retain the full [`TopologySnapshot::node_count`].
pub const MAX_TOPOLOGY_NODES: usize = 128;

impl TopologyConfig {
    /// Peers remain unknown; the local member alone carries its observed frontier.
    #[must_use]
    pub fn snapshot(&self, view: TopologyView, local: LocalStatus, captured_at: i64) -> TopologySnapshot {
        let operator = view.admits(TopologyView::Operator);
        let administrator = view.admits(TopologyView::Administrator);
        let nodes = self
            .members
            .iter()
            .take(MAX_TOPOLOGY_NODES)
            .map(|member| {
                let is_local = self.local_node.as_deref() == Some(member.node.as_str());
                let liveness = if is_local {
                    local.liveness
                } else {
                    NodeLiveness::Unknown
                };
                TopologyNode {
                    node: member.node.clone(),
                    dc: member.dc.clone(),
                    role: member.role,
                    local: is_local,
                    liveness: operator.then_some(liveness),
                    frontier: (operator && is_local).then_some(local.frontier),
                    address: administrator.then(|| member.address.clone()),
                }
            })
            .collect();
        TopologySnapshot {
            mode: self.mode,
            group: self.group.clone(),
            captured_at,
            node_count: self.members.len(),
            local: LocalNode {
                role: local.role,
                liveness: operator.then_some(local.liveness),
                frontier: operator.then_some(local.frontier),
            },
            nodes,
        }
    }
}

/// Always present and never [`NodeLiveness::Unknown`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalNode {
    pub role: NodeRole,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub liveness: Option<NodeLiveness>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frontier: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologyNode {
    pub node: String,
    pub dc: String,
    pub role: NodeRole,
    pub local: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub liveness: Option<NodeLiveness>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frontier: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
}

/// Captures one caller-filtered, size-capped view without exposing live state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologySnapshot {
    pub mode: TopologyMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Lets callers detect stale snapshots.
    pub captured_at: i64,
    /// May exceed `nodes.len()` when the roster is capped.
    pub node_count: usize,
    pub local: LocalNode,
    pub nodes: Vec<TopologyNode>,
}

#[cfg(test)]
#[path = "../tests/unit/topology/tests.rs"]
mod tests;
