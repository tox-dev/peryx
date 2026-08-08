use leptos::prelude::*;
use peryx_core::{LocalNode, NodeLiveness, NodeRole, TopologyMode, TopologyNode, TopologySnapshot};

use super::{RoleFilter, StreamStatus, topology_view};

fn node(name: &str, role: NodeRole, address: Option<&str>) -> TopologyNode {
    TopologyNode {
        node: name.to_owned(),
        dc: "dc".to_owned(),
        role,
        local: false,
        liveness: Some(NodeLiveness::Unknown),
        frontier: Some(1),
        address: address.map(str::to_owned),
    }
}

fn writer_only() -> TopologySnapshot {
    TopologySnapshot {
        mode: TopologyMode::None,
        group: Some("alpha".to_owned()),
        captured_at: 0,
        node_count: 1,
        local: LocalNode {
            role: NodeRole::Writer,
            liveness: Some(NodeLiveness::Live),
            frontier: Some(1),
        },
        nodes: vec![node("writer-a", NodeRole::Writer, None)],
    }
}

fn capped_ha_replicas() -> TopologySnapshot {
    TopologySnapshot {
        mode: TopologyMode::Ha,
        group: Some("beta".to_owned()),
        captured_at: 0,
        node_count: 5,
        local: LocalNode {
            role: NodeRole::Writer,
            liveness: Some(NodeLiveness::Live),
            frontier: Some(1),
        },
        nodes: vec![
            node("replica-b", NodeRole::Replica, Some("replica-b.internal:8443")),
            node("replica-c", NodeRole::Replica, Some("replica-c.internal:8443")),
        ],
    }
}

fn render(
    live: RwSignal<TopologySnapshot>,
    filter: ReadSignal<RoleFilter>,
    set_filter: WriteSignal<RoleFilter>,
) -> Memo<String> {
    let status = RwSignal::new(StreamStatus::Live);
    let streaming = RwSignal::new(false);
    Memo::new(move |_| topology_view(live, filter, set_filter, status, streaming).to_html())
}

#[test]
fn test_topology_page_follows_a_streamed_snapshot() {
    Owner::new().with(|| {
        let live = RwSignal::new(writer_only());
        let (filter, set_filter) = signal(RoleFilter::All);
        let html = render(live, filter, set_filter);

        let first = html.get();
        assert!(first.contains("Single node"), "{first}");
        assert!(first.contains("alpha"), "{first}");
        assert!(first.contains("of 1 roster nodes."), "{first}");
        assert!(!first.contains("High availability"), "{first}");
        assert!(!first.contains("capped per snapshot"), "{first}");
        assert!(!first.contains(">Address<"), "{first}");
        assert!(!first.contains("replica-b.internal:8443"), "{first}");

        live.set(capped_ha_replicas());

        let second = html.get();
        assert!(second.contains("High availability"), "{second}");
        assert!(second.contains("beta"), "{second}");
        assert!(second.contains("of 5 roster nodes."), "{second}");
        assert!(second.contains("Showing 2 of 5 nodes."), "{second}");
        assert!(second.contains(">Address<"), "{second}");
        assert!(second.contains("replica-b.internal:8443"), "{second}");
        assert!(!second.contains("Single node"), "{second}");
        assert!(!second.contains("alpha"), "{second}");
    });
}

#[test]
fn test_topology_roster_filters_to_the_selected_role() {
    Owner::new().with(|| {
        let live = RwSignal::new(capped_ha_replicas());
        let (filter, set_filter) = signal(RoleFilter::All);
        let html = render(live, filter, set_filter);

        assert!(html.get().contains("replica-b<"), "{}", html.get());

        set_filter.set(RoleFilter::Writer);

        let filtered = html.get();
        assert!(!filtered.contains("replica-b<"), "{filtered}");
        assert!(filtered.contains("Showing 0 of 5 roster nodes."), "{filtered}");
    });
}

#[test]
fn test_topology_page_reports_a_standalone_node() {
    Owner::new().with(|| {
        let live = RwSignal::new(TopologySnapshot {
            mode: TopologyMode::None,
            group: None,
            captured_at: 0,
            node_count: 0,
            local: LocalNode {
                role: NodeRole::Writer,
                liveness: Some(NodeLiveness::Live),
                frontier: Some(0),
            },
            nodes: Vec::new(),
        });
        let (filter, set_filter) = signal(RoleFilter::All);
        let html = render(live, filter, set_filter).get();

        assert!(html.contains("runs standalone"), "{html}");
        assert!(html.contains("Single node"), "{html}");
        assert!(html.contains('-'), "{html}");
        assert!(!html.contains("id=\"topology-role\""), "{html}");
    });
}
