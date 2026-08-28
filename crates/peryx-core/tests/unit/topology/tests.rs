use super::{
    LocalStatus, MAX_TOPOLOGY_NODES, NodeLiveness, NodeRole, TopologyConfig, TopologyMember, TopologyMode,
    TopologyNode, TopologySnapshot, TopologyView,
};
use rstest::rstest;

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

#[rstest]
#[case::without_local(None)]
#[case::local_before_cap(Some(5))]
#[case::local_after_cap(Some(MAX_TOPOLOGY_NODES + 4))]
#[case::missing_local(Some(MAX_TOPOLOGY_NODES + 5))]
fn test_snapshot_caps_the_roster_and_retains_a_matching_local(#[case] local_index: Option<usize>) {
    let member_count = MAX_TOPOLOGY_NODES + 5;
    let mut retained: Vec<_> = (0..MAX_TOPOLOGY_NODES).collect();
    if let Some(local_index) = local_index.filter(|index| *index >= MAX_TOPOLOGY_NODES && *index < member_count) {
        retained[MAX_TOPOLOGY_NODES - 1] = local_index;
    }
    let expected = retained
        .into_iter()
        .map(|index| {
            let is_local = Some(index) == local_index;
            TopologyNode {
                node: format!("node-{index}"),
                dc: format!("dc-{index}"),
                role: NodeRole::Replica,
                local: is_local,
                liveness: Some(if is_local {
                    NodeLiveness::Live
                } else {
                    NodeLiveness::Unknown
                }),
                frontier: is_local.then_some(42),
                address: None,
            }
        })
        .collect();
    let snapshot = TopologyConfig {
        mode: TopologyMode::Ha,
        group: Some("wide".to_owned()),
        members: (0..member_count)
            .map(|index| member(&format!("node-{index}"), &format!("dc-{index}"), NodeRole::Replica))
            .collect(),
        local_node: local_index.map(|index| format!("node-{index}")),
    }
    .snapshot(TopologyView::Operator, local(), 0);
    assert_eq!((snapshot.node_count, snapshot.nodes), (member_count, expected));
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
