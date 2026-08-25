use leptos::prelude::*;
use peryx_core::{LocalNode, NodeLiveness, NodeRole, TopologyMode, TopologyNode, TopologySnapshot};

use super::{RoleFilter, StreamStatus, TopologyBody, loaded_topology, topology_view};

fn node(name: &str, role: NodeRole, local: bool, frontier: Option<u64>, address: Option<&str>) -> TopologyNode {
    TopologyNode {
        node: name.to_owned(),
        dc: "dc".to_owned(),
        role,
        local,
        liveness: Some(NodeLiveness::Unknown),
        frontier,
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
        nodes: vec![node("writer-a", NodeRole::Writer, true, Some(1), None)],
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
            node(
                "replica-b",
                NodeRole::Replica,
                false,
                Some(1),
                Some("replica-b.internal:8443"),
            ),
            node("replica-c", NodeRole::Replica, false, None, None),
        ],
    }
}

fn render(
    live: RwSignal<TopologySnapshot>,
    filter: ReadSignal<RoleFilter>,
    set_filter: WriteSignal<RoleFilter>,
    status: RwSignal<StreamStatus>,
    streaming: RwSignal<bool>,
) -> String {
    topology_view(live, filter, set_filter, status, streaming)
        .to_html()
        .replace("<!>", "")
}

#[test]
fn test_topology_page_follows_a_streamed_snapshot() {
    Owner::new().with(|| {
        let live = RwSignal::new(writer_only());
        let (filter, set_filter) = signal(RoleFilter::All);
        let status = RwSignal::new(StreamStatus::Live);
        let streaming = RwSignal::new(false);
        let first = render(live, filter, set_filter, status, streaming);

        assert!(first.contains("Single node"));
        assert!(first.contains("alpha"));
        assert!(first.contains("of 1 roster nodes."));
        assert!(!first.contains("High availability"));
        assert!(!first.contains("capped per snapshot"));
        assert!(!first.contains(">Address<"));
        assert!(!first.contains("replica-b.internal:8443"));
        assert!(!first.contains("feed:"));
        assert!(first.contains("topology-self"));

        live.set(capped_ha_replicas());

        let second = render(live, filter, set_filter, status, streaming);
        assert!(second.contains("High availability"));
        assert!(second.contains("beta"));
        assert!(second.contains("of 5 roster nodes."));
        assert!(second.contains("Showing 2 of 5 nodes."));
        assert!(second.contains(">Address<"));
        assert!(second.contains("replica-b.internal:8443"));
        assert!(second.contains("<td class=\"num\">-</td>"));
        assert!(second.contains("<td>-</td>"));
        assert!(!second.contains("Single node"));
        assert!(!second.contains("alpha"));
        assert!(!second.contains("topology-self"));
    });
}

#[test]
fn test_topology_roster_filters_to_the_selected_role() {
    Owner::new().with(|| {
        let live = RwSignal::new(capped_ha_replicas());
        let (filter, set_filter) = signal(RoleFilter::All);
        assert!(
            render(
                live,
                filter,
                set_filter,
                RwSignal::new(StreamStatus::Live),
                RwSignal::new(false),
            )
            .contains("replica-b<")
        );

        set_filter.set(RoleFilter::Writer);

        let filtered = render(
            live,
            filter,
            set_filter,
            RwSignal::new(StreamStatus::Live),
            RwSignal::new(false),
        );
        assert!(!filtered.contains("replica-b<"));
        assert!(filtered.contains("Showing 0 of 5 roster nodes."));
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
                frontier: None,
            },
            nodes: Vec::new(),
        });
        let (filter, set_filter) = signal(RoleFilter::All);
        let html = render(
            live,
            filter,
            set_filter,
            RwSignal::new(StreamStatus::Live),
            RwSignal::new(false),
        );

        assert!(html.contains("runs standalone"));
        assert!(html.contains("Single node"));
        assert!(html.contains("<strong>-</strong>"));
        assert!(html.contains("<span>group</span>"));
        assert!(html.contains("restricted"));
        assert!(!html.contains("id=\"topology-role\""));
    });
}

#[test]
fn test_topology_ssr_omits_stream_badge() {
    Owner::new().with(|| {
        let html = view! { <TopologyBody snapshot=writer_only() /> }.to_html();

        assert!(!html.contains("feed:"));
        assert!(html.contains("writer-a"));
    });
}

#[test]
fn test_topology_stream_badge_follows_the_feed_status() {
    Owner::new().with(|| {
        let live = RwSignal::new(writer_only());
        let (filter, set_filter) = signal(RoleFilter::All);
        let status = RwSignal::new(StreamStatus::Live);
        let streaming = RwSignal::new(true);
        let live_html = render(live, filter, set_filter, status, streaming);
        assert!(live_html.contains("feed: Live"));

        status.set(StreamStatus::Offline);

        let offline_html = render(live, filter, set_filter, status, streaming);
        assert!(offline_html.contains("feed: Offline"));
        assert!(!offline_html.contains("feed: Live"));
    });
}

#[test]
fn test_loaded_topology_renders_success_and_error_results() {
    Owner::new().with(|| {
        for (result, expected) in [
            (Ok(writer_only()), &["writer-a"][..]),
            (Err("load failed".to_owned()), &[r#"role="alert""#, "load failed"][..]),
        ] {
            assert_rendered_contains(&loaded_topology(result).to_html(), expected);
        }
    });
}

fn assert_rendered_contains(html: &str, expected: &[&str]) {
    let rendered = html.replace("<!>", "");
    for value in expected {
        assert!(rendered.contains(value), "missing {value:?} in {rendered}");
    }
}
