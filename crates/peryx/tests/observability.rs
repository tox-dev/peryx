#![cfg(feature = "availability-e2e")]

mod harness;

use harness::{ADMIN_PASSWORD, ADMIN_USER, MemberSpec, ProcessHarness, Role, Topology, cargo_binary};

fn process_harness() -> ProcessHarness {
    ProcessHarness::new(cargo_binary("peryx"))
}

const GENERAL_SERIES: &str = "peryx_pages_served_total";
const AVAILABILITY_SERIES: &str = "peryx_ha_distributed_";
const DC_DURABILITY_SERIES: &str = "peryx_dc_ack_durable_total";

fn dc_group() -> Topology {
    Topology::dc(
        "east",
        vec![
            MemberSpec::new("writer-a", "east-1", Role::Writer),
            MemberSpec::new("replica-b", "east-2", Role::Replica),
        ],
    )
    .with_process_harness(process_harness())
}

fn ha_group() -> Topology {
    Topology::ha(
        "global",
        vec![
            MemberSpec::new("writer-east", "east", Role::Writer),
            MemberSpec::new("replica-west", "west", Role::Replica),
        ],
    )
    .with_process_harness(process_harness())
}

#[test]
fn test_none_metrics_exposes_general_series_and_no_availability_series() {
    let cluster = Topology::single()
        .with_process_harness(process_harness())
        .start()
        .expect("none cluster starts");
    let (code, body) = cluster.nodes()[0].metrics().expect("metrics reachable");

    assert_eq!(code, 200);
    assert!(
        body.contains(GENERAL_SERIES),
        "general metrics present in none mode: {body}"
    );
    assert!(
        !body.contains(AVAILABILITY_SERIES),
        "a none node exports no availability series: {body}",
    );
    assert!(
        !body.contains(DC_DURABILITY_SERIES),
        "a none node runs no datacenter durability decision, so it exports no such series: {body}",
    );
}

#[test]
fn test_dc_metrics_exposes_the_durability_outcome_series() {
    let cluster = dc_group().start().expect("dc cluster starts");
    let (code, body) = cluster
        .node("writer-a")
        .expect("writer is present")
        .metrics()
        .expect("metrics reachable");

    assert_eq!(code, 200);
    assert!(
        body.contains(DC_DURABILITY_SERIES),
        "a dc node exposes the datacenter durability outcome series: {body}",
    );
}

#[test]
fn test_none_mode_mounts_no_availability_routes() {
    let cluster = Topology::single()
        .with_process_harness(process_harness())
        .with_admin()
        .start()
        .expect("none cluster starts");
    let node = &cluster.nodes()[0];

    for (label, code) in [
        ("topology", node.topology().unwrap().0),
        ("placements", node.placements().unwrap().0),
        (
            "authenticated topology",
            node.http_get_as(ADMIN_USER, ADMIN_PASSWORD, "/+availability/topology")
                .unwrap()
                .0,
        ),
        (
            "authenticated placements",
            node.http_get_as(ADMIN_USER, ADMIN_PASSWORD, "/+availability/placements")
                .unwrap()
                .0,
        ),
    ] {
        assert_eq!(code, 404, "{label}");
    }
}

#[test]
fn test_dc_topology_reports_the_roster_with_roles() {
    let cluster = dc_group().start().expect("dc cluster starts");
    let (code, body) = cluster.nodes()[0].topology().expect("topology reachable");

    assert_eq!(code, 200);
    assert!(body.contains("\"mode\":\"dc\""), "topology reports dc mode: {body}");
    assert!(
        body.contains("writer-a") && body.contains("replica-b"),
        "the roster members render: {body}",
    );
    assert!(
        body.contains("\"role\":\"writer\"") && body.contains("\"role\":\"replica\""),
        "each member's role renders: {body}",
    );
}

#[test]
fn test_ha_topology_reports_the_multi_datacenter_roster() {
    let cluster = ha_group().start().expect("ha cluster starts");
    let (code, body) = cluster.nodes()[0].topology().expect("topology reachable");

    assert_eq!(code, 200);
    assert!(body.contains("\"mode\":\"ha\""), "topology reports ha mode: {body}");
    assert!(
        body.contains("east") && body.contains("west"),
        "members in distinct datacenters render: {body}",
    );
}

fn assert_authenticated_views(node: &harness::Node) {
    let (code, placements) = node
        .http_get_as(ADMIN_USER, ADMIN_PASSWORD, "/+availability/placements")
        .expect("placements reachable");
    assert_eq!(code, 200, "an administrator reads the placement view: {placements}");
    assert!(placements.contains("\"health\""), "the aggregate renders: {placements}");
    assert!(placements.contains("\"total\""), "the total renders: {placements}");
    assert!(
        placements.contains("\"rows\""),
        "an administrator reads the per-digest rows: {placements}"
    );

    let (code, topology) = node
        .http_get_as(ADMIN_USER, ADMIN_PASSWORD, "/+availability/topology")
        .expect("topology reachable");
    assert_eq!(code, 200);
    assert!(
        topology.contains("\"liveness\""),
        "an authorized caller reads liveness: {topology}"
    );
    assert!(
        topology.contains("\"frontier\""),
        "an authorized caller reads the frontier: {topology}"
    );
}

#[test]
fn test_dc_authenticated_views_expose_class_filtered_fields() {
    let cluster = dc_group().with_admin().start().expect("dc cluster starts");
    assert_authenticated_views(cluster.node("writer-a").expect("writer is present"));
}

#[test]
fn test_ha_authenticated_views_expose_class_filtered_fields() {
    let cluster = ha_group().with_admin().start().expect("ha cluster starts");
    assert_authenticated_views(cluster.node("writer-east").expect("writer is present"));
}
