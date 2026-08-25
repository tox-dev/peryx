use std::time::Duration;

use peryx_storage::blob::Digest;
use serde_json::{Value, json};

use crate::harness::{
    ADMIN_PASSWORD, ADMIN_USER, Cluster, MemberSpec, Node, ProcessHarness, Role, Topology, Toxiproxy, cargo_binary,
};
use crate::pypi_support::{PypiNodeExt as _, WHEEL, WHEEL_FILENAME, config as pypi_config};

const CONVERGE: Duration = Duration::from_mins(1);
const AGE_OUT: Duration = Duration::from_secs(90);
const LAG: Duration = Duration::from_secs(20);

fn writer_and_replica() -> Topology {
    Topology::ha(
        "east",
        vec![
            MemberSpec::new("writer-a", "east-1", Role::Writer),
            MemberSpec::new("replica-b", "east-2", Role::Replica),
        ],
    )
    .with_admin()
    .with_index_config(&pypi_config().replace("projects", "resources"))
    .with_process_harness(ProcessHarness::new(cargo_binary("peryx-pypi-system-server")))
}

fn readiness_document(writer: &Node) -> Value {
    let (_, body) = writer
        .http_get_as(ADMIN_USER, ADMIN_PASSWORD, "/+replication/v1/ready")
        .expect("the writer's replication readiness is reachable");
    serde_json::from_str(&body).expect("the readiness document is json")
}

fn await_readiness(writer: &Node, within: Duration, settled: impl Fn(&Value) -> bool) -> Value {
    writer
        .await_topology_signal(within, |writer| {
            let document = readiness_document(writer);
            let failure = format!("the writer's readiness never reached the asserted state:\n{document:#}");
            (settled(&document).then_some(document), failure)
        })
        .expect("the writer signals its readiness transition")
}

fn group_ready(document: &Value) -> Option<bool> {
    document["group_readiness"]["ready"].as_bool()
}

fn durable_frontier(document: &Value) -> Option<u64> {
    document["group_readiness"]["durable_frontier"].as_u64()
}

fn writer_serial(document: &Value) -> Option<u64> {
    document["serial"].as_u64()
}

fn pending_upload() -> (u16, String) {
    (
        202,
        format!(
            "upload accepted; durability pending, retry-safe operation pypi:hosted:veloxdemo:{WHEEL_FILENAME}:{}",
            Digest::of(WHEEL).as_str(),
        ),
    )
}

fn index_of(cluster: &Cluster, identity: &str) -> usize {
    cluster
        .nodes()
        .iter()
        .position(|node| node.identity() == identity)
        .expect("the requested node is present")
}

#[test]
fn test_group_readiness_converges_as_a_replica_joins_after_the_writer() {
    let mut toxiproxy = Toxiproxy::start().expect("toxiproxy starts");
    let proxied = writer_and_replica()
        .start_proxied(&mut toxiproxy, false)
        .expect("the dc group starts with the replica partitioned");
    let writer = proxied.cluster().node("writer-a").expect("the writer is present");

    let blocked = await_readiness(writer, CONVERGE, |document| group_ready(document) == Some(false));
    let group = &blocked["group_readiness"];
    assert_eq!(group["blocked"]["insufficient_members"]["reporting"], json!(1));
    assert_eq!(group["blocked"]["insufficient_members"]["required"], json!(2));
    assert_eq!(durable_frontier(&blocked), Some(0));

    proxied
        .proxy("replica-b")
        .expect("the replica has a proxy")
        .heal()
        .expect("heal the replica's link to the writer");

    let ready = await_readiness(writer, CONVERGE, |document| group_ready(document) == Some(true));
    assert_eq!(ready["group_readiness"]["blocked"], Value::Null);
    assert_eq!(durable_frontier(&ready), Some(0));
}

#[test]
fn test_group_readiness_recovers_after_a_replica_is_killed_and_restarted() {
    let mut cluster = writer_and_replica().start().expect("the dc group starts");
    let writer = index_of(&cluster, "writer-a");
    let replica = index_of(&cluster, "replica-b");

    await_readiness(&cluster.nodes()[writer], CONVERGE, |document| {
        group_ready(document) == Some(true)
    });

    cluster.nodes_mut()[replica].kill();
    let blocked = await_readiness(&cluster.nodes()[writer], AGE_OUT, |document| {
        group_ready(document) == Some(false)
    });
    assert_eq!(
        blocked["group_readiness"]["blocked"]["insufficient_members"]["reporting"],
        json!(1),
    );

    cluster.nodes_mut()[replica]
        .restart()
        .expect("the replica restarts on its port");
    await_readiness(&cluster.nodes()[writer], CONVERGE, |document| {
        group_ready(document) == Some(true)
    });
}

#[test]
fn test_a_slow_replica_reports_but_holds_the_durable_frontier_at_its_applied_serial() {
    let mut toxiproxy = Toxiproxy::start().expect("toxiproxy starts");
    let proxied = writer_and_replica()
        .start_proxied(&mut toxiproxy, true)
        .expect("the dc group starts");
    let writer = proxied.cluster().node("writer-a").expect("the writer is present");

    await_readiness(writer, CONVERGE, |document| {
        group_ready(document) == Some(true)
            && durable_frontier(document) == Some(0)
            && writer_serial(document) == Some(0)
    });

    let proxy = proxied.proxy("replica-b").expect("the replica has a proxy");
    proxy.pause(LAG).expect("slow the replica's link to the writer");

    let (code, body) = writer.publish().expect("the publish reaches the writer");
    assert_eq!((code, body), pending_upload());

    let lagging = await_readiness(writer, CONVERGE, |document| {
        writer_serial(document).is_some_and(|serial| serial > 0)
            && group_ready(document) == Some(true)
            && durable_frontier(document).is_some()
    });
    let published = writer_serial(&lagging).expect("the poll guaranteed a serial");
    let frontier = durable_frontier(&lagging).expect("the poll guaranteed a durable frontier");
    assert!(
        frontier < published,
        "the durable frontier must not pass the lagging replica: {lagging:#}",
    );

    proxy.resume().expect("restore the replica's link to the writer");
    await_readiness(writer, CONVERGE, |document| {
        durable_frontier(document) == Some(published)
    });
}

#[test]
fn test_killing_the_writer_stops_writes_and_preserves_the_durable_frontier() {
    let mut cluster = writer_and_replica().start().expect("the dc group starts");
    let writer = index_of(&cluster, "writer-a");
    let replica = index_of(&cluster, "replica-b");

    await_readiness(&cluster.nodes()[writer], CONVERGE, |document| {
        group_ready(document) == Some(true)
    });
    let published = cluster.nodes()[writer]
        .publish()
        .expect("the publish reaches the writer");
    assert_eq!(published, pending_upload());
    let settled = await_readiness(&cluster.nodes()[writer], CONVERGE, |document| {
        group_ready(document) == Some(true)
            && writer_serial(document).is_some_and(|serial| serial > 0)
            && durable_frontier(document) == writer_serial(document)
    });
    let frontier = writer_serial(&settled).expect("the writer reports a serial");

    cluster.nodes_mut()[writer].kill();
    assert_eq!(
        cluster.nodes()[replica]
            .publish()
            .expect("the upload reaches the replica"),
        (
            503,
            r#"{"error":"read_only_replica","message":"this replica does not accept mutations"}"#.to_owned(),
        ),
    );
    assert!(
        cluster.nodes()[writer].publish().is_err(),
        "the dead writer accepts no write",
    );

    cluster.nodes_mut()[writer]
        .restart()
        .expect("the writer restarts on its port");
    let recovered = await_readiness(&cluster.nodes()[writer], CONVERGE, |document| {
        group_ready(document) == Some(true) && durable_frontier(document) == Some(frontier)
    });
    assert_eq!(
        writer_serial(&recovered),
        Some(frontier),
        "no write advanced the serial while the writer was down: {recovered:#}",
    );
}
