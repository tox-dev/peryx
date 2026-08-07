//! Observability across availability modes: the metrics, topology, and placement surfaces render
//! correctly under `none`, `dc`, and `ha` configuration, over a group of real `peryx serve` processes.
//!
//! The `none` cases assert what a single node exposes today. The `dc` and `ha` cases spawn a live group
//! and assert its observability surfaces, which is what the peer-RPC router mount enables, so they ship
//! with that functionality rather than as a standalone suite.
//!
//! Gated behind the `availability-e2e` feature so the default `cargo test` and the coverage gate skip
//! them: they spawn processes and need the `toxiproxy-server` binary. CI runs them in a dedicated job.

#![cfg(feature = "availability-e2e")]

mod harness;

use harness::{ADMIN_PASSWORD, ADMIN_USER, MemberSpec, Role, Topology};

/// A general metric family every node exports regardless of availability mode.
const GENERAL_SERIES: &str = "peryx_pages_served_total";

/// The metric family a replica exports once it replicates. A `none` node runs no replication and so
/// exports none of it, which is how `none` is told apart from an enabled mode on the scrape alone.
const AVAILABILITY_SERIES: &str = "peryx_ha_distributed_";

/// The datacenter durability outcome family a `dc` or `ha` node exports. A `none` node runs no such
/// decision, so it exports none of it.
const DC_DURABILITY_SERIES: &str = "peryx_dc_ack_durable_total";

fn dc_group() -> Topology {
    Topology::dc(
        "east",
        vec![
            MemberSpec::new("writer-a", "east-1", Role::Writer),
            MemberSpec::new("replica-b", "east-2", Role::Replica),
        ],
    )
}

fn ha_group() -> Topology {
    Topology::ha(
        "global",
        vec![
            MemberSpec::new("writer-east", "east", Role::Writer),
            MemberSpec::new("replica-west", "west", Role::Replica),
        ],
    )
}

#[test]
fn test_none_metrics_exposes_general_series_and_no_availability_series() {
    let cluster = Topology::single().start().expect("none cluster starts");
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
fn test_none_topology_reports_the_single_node_view() {
    let cluster = Topology::single().start().expect("none cluster starts");
    let (code, body) = cluster.nodes()[0].topology().expect("topology reachable");

    assert_eq!(code, 200);
    assert!(body.contains("\"mode\":\"none\""), "topology reports none mode: {body}");
}

#[test]
fn test_placements_fails_closed_for_an_anonymous_caller() {
    // The placement view is operator-gated, so an unauthenticated scrape proves the endpoint is mounted
    // and refuses rather than leaking a digest. The refusal holds in every mode.
    let cluster = Topology::single().start().expect("none cluster starts");
    let (code, _) = cluster.nodes()[0].placements().expect("placements reachable");

    assert_eq!(code, 403, "an anonymous placement read is forbidden");
}

// A replica exporting its availability series is asserted with the replica-bootstrap capability the
// harness still lacks (it cannot claim a replica's writer identity offline), so those scenarios land
// with that feature rather than here.

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

/// Assert the authenticated observability views on one node of `cluster`: an administrator reads the
/// placement aggregate and its per-digest rows, and reads the operator-class liveness and frontier the
/// anonymous topology view withholds. Bundling both reads onto one spawned group keeps the per-mode
/// authenticated coverage inside the harness runtime budget.
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
fn test_none_authenticated_views_expose_class_filtered_fields() {
    let cluster = Topology::single().with_admin().start().expect("none cluster starts");
    assert_authenticated_views(&cluster.nodes()[0]);
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
