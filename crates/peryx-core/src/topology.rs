//! The neutral availability-topology snapshot the operator surfaces render.
//!
//! An operator page needs one immutable picture of the availability group - the mode, the configured
//! roster, and this node's own live frontier - taken at a single instant, instead of traversing live
//! membership and storage state on every poll. [`TopologyConfig`] is the fixed input a process holds in
//! its serving state; [`TopologyConfig::snapshot`] projects it to one caller's [`TopologyView`] and
//! stamps it with the observation time, so a stale render shows as age rather than passing for health.
//!
//! A peer's liveness and frontier are [`NodeLiveness::Unknown`] here: this node observes only itself
//! until a consensus layer reports peers, and an unknown peer must never read as healthy. This
//! placeholder is not the writer's beacon view of its replicas: a `dc`/`ha` writer already ages replica
//! heartbeats into `alive`/`suspect`/`dead` on the `peers` field of its `/+replication/v1/ready`
//! document, which is the authoritative peer-liveness source until the consensus layer lands. The models
//! are pure serde with no I/O or authority logic, so they cross the server/browser boundary and pull no
//! auth or storage type into a renderer.

use serde::{Deserialize, Serialize};

/// The availability mode a node runs under, mirrored into a snapshot without a configuration dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum TopologyMode {
    /// Single writer, local durability, operator-driven failover.
    #[default]
    None,
    /// A configured writer and explicit read replicas within one datacenter.
    Dc,
    /// Metadata durability in a remote datacenter.
    Ha,
}

/// A node's fixed role in its configured group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeRole {
    /// The single node that accepts authoritative writes.
    Writer,
    /// A read replica that applies the writer's changes.
    Replica,
}

/// What is known about a node's live participation at the instant a snapshot was taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NodeLiveness {
    /// The node serves reads at its frontier.
    Live,
    /// The node is up but its local stores cannot serve.
    Unready,
    /// No live observation exists, so the node's health is unknown and must not read as healthy.
    Unknown,
}

/// The least authority a snapshot field requires, so a snapshot serializes only what a caller may read.
///
/// The roster identities, datacenters, and roles are [`Public`](Self::Public); live frontiers and
/// liveness need [`Operator`](Self::Operator); the advertised peer addresses need
/// [`Administrator`](Self::Administrator). The order is a strict hierarchy, so an administrator reads
/// every field and a public caller reads the fewest.
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

    /// Whether a caller at this view may read a field that requires `least`.
    #[must_use]
    pub const fn admits(self, least: Self) -> bool {
        self.rank() >= least.rank()
    }
}

/// One configured member of the group, as this node was told about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopologyMember {
    /// The member's stable identity, unique within the group.
    pub node: String,
    /// The datacenter the member runs in.
    pub dc: String,
    /// The address peers reach the member on, revealed only to an administrator.
    pub address: String,
    pub role: NodeRole,
}

/// This process's own live self-observation, read when a snapshot is taken.
///
/// A node always knows its own role, store health, and metadata frontier, so this is the one part of a
/// snapshot never [`NodeLiveness::Unknown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalStatus {
    pub role: NodeRole,
    pub liveness: NodeLiveness,
    /// The metadata serial this node has committed.
    pub frontier: u64,
}

/// The fixed availability topology a process was configured with.
///
/// Held in serving state and projected into a [`TopologySnapshot`] per request. It names no live frontier
/// or observation time; those are read when a snapshot is taken.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TopologyConfig {
    pub mode: TopologyMode,
    /// The group identity, absent under single-node `none` mode and whenever no roster is configured.
    pub group: Option<String>,
    pub members: Vec<TopologyMember>,
    /// The identity of the roster member this process runs as, when the process knows it. A writer knows
    /// its own identity; a replica does not carry its roster identity, so it leaves this unset.
    pub local_node: Option<String>,
}

impl TopologyConfig {
    /// This process's own datacenter, the one holding the roster member it names through
    /// [`local_node`](Self::local_node). `None` when the process names no local member - a rosterless
    /// single node, or a replica that carries no roster identity - so a caller supplies its own fallback.
    /// Every per-node decision that needs the local datacenter resolves it here, so the roster lookup lives
    /// in one place rather than being re-derived at each call site.
    #[must_use]
    pub fn local_datacenter(&self) -> Option<&str> {
        let local = self.local_node.as_deref()?;
        self.members
            .iter()
            .find(|member| member.node == local)
            .map(|member| member.dc.as_str())
    }
}

/// The most nodes a snapshot serializes, so one request cannot return an unbounded roster. A larger
/// group still reports its full [`TopologySnapshot::node_count`], so truncation stays visible.
pub const MAX_TOPOLOGY_NODES: usize = 128;

impl TopologyConfig {
    /// Project this topology to one caller's view at `captured_at`, reading `local` for this node's own
    /// live status. Peers report [`NodeLiveness::Unknown`] with no frontier; only the local roster member
    /// carries the live frontier. Fields above the caller's view are omitted, and the node list is capped
    /// at [`MAX_TOPOLOGY_NODES`] while `node_count` keeps the true size.
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

/// This process's own node in a snapshot: always present and never [`NodeLiveness::Unknown`], because a
/// node always observes itself, even when no roster names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalNode {
    pub role: NodeRole,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub liveness: Option<NodeLiveness>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frontier: Option<u64>,
}

/// One roster node in a snapshot, reduced to the fields the caller's [`TopologyView`] admits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologyNode {
    pub node: String,
    pub dc: String,
    pub role: NodeRole,
    /// Whether this member is the process that produced the snapshot.
    pub local: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub liveness: Option<NodeLiveness>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frontier: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
}

/// An immutable availability-topology snapshot an operator surface renders instead of traversing live
/// membership and storage state. Taken at one instant, reduced to one caller's view, and capped in size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologySnapshot {
    pub mode: TopologyMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Unix seconds when the snapshot was taken, so a stale render is visible as age.
    pub captured_at: i64,
    /// The total configured roster size, which may exceed the capped `nodes` list.
    pub node_count: usize,
    pub local: LocalNode,
    pub nodes: Vec<TopologyNode>,
}

#[cfg(test)]
mod tests {
    use super::{
        LocalStatus, MAX_TOPOLOGY_NODES, NodeLiveness, NodeRole, TopologyConfig, TopologyMember, TopologyMode,
        TopologyNode, TopologySnapshot, TopologyView,
    };

    fn member(node: &str, dc: &str, role: NodeRole) -> TopologyMember {
        TopologyMember {
            node: node.to_owned(),
            dc: dc.to_owned(),
            address: format!("{node}:8080"),
            role,
        }
    }

    fn writer_group() -> TopologyConfig {
        TopologyConfig {
            mode: TopologyMode::Dc,
            group: Some("east".to_owned()),
            members: vec![
                member("writer-a", "east-1", NodeRole::Writer),
                member("replica-b", "east-2", NodeRole::Replica),
            ],
            local_node: Some("writer-a".to_owned()),
        }
    }

    fn local() -> LocalStatus {
        LocalStatus {
            role: NodeRole::Writer,
            liveness: NodeLiveness::Live,
            frontier: 42,
        }
    }

    fn snapshot(view: TopologyView) -> TopologySnapshot {
        writer_group().snapshot(view, local(), 1_800_000_000)
    }

    fn node<'a>(snapshot: &'a TopologySnapshot, identity: &str) -> &'a TopologyNode {
        snapshot.nodes.iter().find(|node| node.node == identity).unwrap()
    }

    #[test]
    fn test_admits_orders_views_as_a_hierarchy() {
        assert!(!TopologyView::Public.admits(TopologyView::Operator));
        assert!(TopologyView::Operator.admits(TopologyView::Operator));
        assert!(TopologyView::Administrator.admits(TopologyView::Operator));
        assert!(!TopologyView::Operator.admits(TopologyView::Administrator));
        assert!(TopologyView::Public.admits(TopologyView::Public));
    }

    #[test]
    fn test_public_view_reveals_only_the_static_roster() {
        let snapshot = snapshot(TopologyView::Public);
        assert_eq!(snapshot.mode, TopologyMode::Dc);
        assert_eq!(snapshot.group.as_deref(), Some("east"));
        assert_eq!(snapshot.node_count, 2);
        assert_eq!(snapshot.local.role, NodeRole::Writer);
        assert_eq!(snapshot.local.liveness, None);
        assert_eq!(snapshot.local.frontier, None);
        let writer = node(&snapshot, "writer-a");
        assert!(writer.local);
        assert_eq!(writer.liveness, None);
        assert_eq!(writer.frontier, None);
        assert_eq!(writer.address, None);
    }

    #[test]
    fn test_operator_view_adds_liveness_and_the_local_frontier_only() {
        let snapshot = snapshot(TopologyView::Operator);
        assert_eq!(snapshot.local.liveness, Some(NodeLiveness::Live));
        assert_eq!(snapshot.local.frontier, Some(42));
        let writer = node(&snapshot, "writer-a");
        assert_eq!(writer.liveness, Some(NodeLiveness::Live));
        assert_eq!(writer.frontier, Some(42));
        assert_eq!(writer.address, None);
        let peer = node(&snapshot, "replica-b");
        assert!(!peer.local);
        assert_eq!(peer.liveness, Some(NodeLiveness::Unknown), "a peer is never live");
        assert_eq!(peer.frontier, None, "a peer frontier is unknown");
    }

    #[test]
    fn test_administrator_view_adds_the_advertised_addresses() {
        let snapshot = snapshot(TopologyView::Administrator);
        assert_eq!(node(&snapshot, "writer-a").address.as_deref(), Some("writer-a:8080"));
        assert_eq!(node(&snapshot, "replica-b").address.as_deref(), Some("replica-b:8080"));
    }

    #[test]
    fn test_snapshot_caps_the_node_list_but_reports_the_full_count() {
        let members = (0..MAX_TOPOLOGY_NODES + 5)
            .map(|index| member(&format!("node-{index}"), &format!("dc-{index}"), NodeRole::Replica))
            .collect();
        let config = TopologyConfig {
            mode: TopologyMode::Ha,
            group: Some("wide".to_owned()),
            members,
            local_node: None,
        };
        let snapshot = config.snapshot(TopologyView::Administrator, local(), 0);
        assert_eq!(snapshot.nodes.len(), MAX_TOPOLOGY_NODES);
        assert_eq!(snapshot.node_count, MAX_TOPOLOGY_NODES + 5);
        assert!(snapshot.nodes.iter().all(|node| !node.local), "no node is local here");
    }

    #[test]
    fn test_none_mode_default_has_no_group_or_roster() {
        let snapshot = TopologyConfig::default().snapshot(
            TopologyView::Administrator,
            LocalStatus {
                role: NodeRole::Replica,
                liveness: NodeLiveness::Unready,
                frontier: 0,
            },
            7,
        );
        assert_eq!(snapshot.mode, TopologyMode::None);
        assert_eq!(snapshot.group, None);
        assert_eq!(snapshot.node_count, 0);
        assert!(snapshot.nodes.is_empty());
        assert_eq!(snapshot.local.role, NodeRole::Replica);
        assert_eq!(snapshot.local.liveness, Some(NodeLiveness::Unready));
    }

    #[test]
    fn test_local_datacenter_reads_the_local_roster_members_dc() {
        assert_eq!(writer_group().local_datacenter(), Some("east-1"));
    }

    #[test]
    fn test_local_datacenter_is_none_without_a_named_local_member() {
        assert_eq!(
            TopologyConfig::default().local_datacenter(),
            None,
            "a rosterless node names none"
        );
        let unlisted = TopologyConfig {
            local_node: Some("ghost".to_owned()),
            ..writer_group()
        };
        assert_eq!(
            unlisted.local_datacenter(),
            None,
            "a local id absent from the roster resolves nothing"
        );
    }

    #[test]
    fn test_snapshot_round_trips_through_json() {
        let snapshot = snapshot(TopologyView::Administrator);
        let encoded = serde_json::to_string(&snapshot).unwrap();
        assert_eq!(serde_json::from_str::<TopologySnapshot>(&encoded).unwrap(), snapshot);
    }
}
