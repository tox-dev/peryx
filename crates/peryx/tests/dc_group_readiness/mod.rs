//! Multi-process proofs that a `dc` group writer's published readiness tracks its replicas' real
//! frontiers ([#970], the multi-process half of [#515]).
//!
//! Three real `peryx serve` binaries are never needed here: a `dc` group is one writer and its
//! replicas, and the readiness surface lives only on the writer, which folds each replica's beaconed
//! frontier into `group_readiness` on `GET /+replication/v1/ready`. So a writer and a single replica
//! exercise the whole surface - a majority group of two needs both members to acknowledge, which makes
//! every transition (a member joining, leaving, lagging, or being lost) observable in the writer's
//! document. The replica beacons its applied serial to the writer over the same link it syncs metadata
//! on, so routing that link through Toxiproxy lets a test slow or cut one replica in isolation.
//!
//! The four faults [#515] left open, each driven through the availability harness and asserted over the
//! writer's public HTTP surface, never a private crate seam:
//!
//! - startup order: a replica that comes up cut off from the writer has not joined; healing the link
//!   converges the writer's readiness to ready as the replica beacons in.
//! - reconnect: a replica killed long enough to age out of the writer's dead window drops the group
//!   below its majority; restarting it returns the group to reporting.
//! - slow replica: a replica whose journal sync is delayed keeps beaconing a lagging applied serial, so
//!   it reads as reporting but never advances the durable frontier past what it holds, until it catches
//!   up.
//! - writer loss: killing the writer stops new writes - the survivor refuses them read-only - while the
//!   durable frontier a quorum already held survives the writer's crash and restart.
//!
//! Gated behind the `availability-e2e` feature so the default `cargo test` and the coverage gate skip
//! them: they spawn real binaries and need the `toxiproxy-server` binary. CI runs them in a dedicated
//! job.
//!
//! This lives in the `availability` test binary, not a target of its own, so the regular `test` job's
//! `not(binary(availability))` nextest filter keeps these heavy multi-process tests off the fast matrix
//! and the dedicated `availability` job - the one that installs `toxiproxy-server` - runs them.
//!
//! [#515]: https://github.com/tox-dev/peryx/issues/515
//! [#970]: https://github.com/tox-dev/peryx/issues/970

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::harness::{ADMIN_PASSWORD, ADMIN_USER, Cluster, MemberSpec, Node, Role, Topology, Toxiproxy};

/// A budget for a live convergence the group reaches on its own: a beacon landing, a replica catching
/// up, a restarted node rejoining. Generous because a saturated CI runner starves the real processes'
/// schedulers, not because the step is slow.
const CONVERGE: Duration = Duration::from_mins(1);
/// A budget for the writer to notice a member has fallen silent. The dead window is 45 seconds, so this
/// clears it with slack for a loaded runner without masking a member that never ages out.
const AGE_OUT: Duration = Duration::from_secs(90);
/// The delay a slow replica's writer→replica traffic runs under: long enough that the replica cannot
/// apply a fresh write for the span of an observation, short enough to stay inside the 45-second dead
/// window so its beacon requests keep it reporting the whole time.
const LAG: Duration = Duration::from_secs(20);

/// A two-member `dc` group - one writer, one replica - that bootstraps an administrator so a test reads
/// the operator-class `serial` and `group_readiness` fields off the writer's readiness document.
fn writer_and_replica() -> Topology {
    Topology::dc(
        "east",
        vec![
            MemberSpec::new("writer-a", "east-1", Role::Writer),
            MemberSpec::new("replica-b", "east-2", Role::Replica),
        ],
    )
    .with_admin()
}

/// The writer's availability readiness document, read as an administrator so the operator-class fields
/// are present. The endpoint answers `200` or `503` for the writer's own node readiness; either way the
/// body carries the group document, so the code is irrelevant and only the body is parsed.
fn readiness_document(writer: &Node) -> Value {
    let (_, body) = writer
        .http_get_as(ADMIN_USER, ADMIN_PASSWORD, "/+replication/v1/ready")
        .expect("the writer's replication readiness is reachable");
    serde_json::from_str(&body).expect("the readiness document is json")
}

/// Poll the writer's readiness document until `settled` holds, then return it, or panic with the last
/// document after `within`. Deterministic in outcome: it waits for the group to reach the asserted state
/// rather than sleeping a fixed span and asserting against a race.
fn await_readiness(writer: &Node, within: Duration, settled: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + within;
    loop {
        let document = readiness_document(writer);
        if settled(&document) {
            return document;
        }
        assert!(
            Instant::now() < deadline,
            "the writer's readiness never reached the asserted state:\n{document:#}",
        );
        std::thread::sleep(Duration::from_millis(100));
    }
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

/// The index of the node with `identity` in the cluster, so a test can kill or restart it through
/// `nodes_mut` while still reading a different node immutably between calls.
fn index_of(cluster: &Cluster, identity: &str) -> usize {
    cluster
        .nodes()
        .iter()
        .position(|node| node.identity() == identity)
        .unwrap_or_else(|| panic!("node {identity} is present"))
}

#[test]
fn test_group_readiness_converges_as_a_replica_joins_after_the_writer() {
    let mut toxiproxy = Toxiproxy::start().expect("toxiproxy starts");
    // Bring the group up with the replica's link to the writer cut. The replica serves read-only but
    // cannot beacon, so it has not joined the group: the writer reports only itself, one short of the
    // two-member majority, and its readiness is blocked on the missing member.
    let proxied = writer_and_replica()
        .start_proxied(&mut toxiproxy, false)
        .expect("the dc group starts with the replica partitioned");
    let writer = proxied.cluster().node("writer-a").expect("the writer is present");

    let blocked = await_readiness(writer, CONVERGE, |document| group_ready(document) == Some(false));
    let group = &blocked["group_readiness"];
    assert_eq!(group["blocked"]["insufficient_members"]["reporting"], json!(1));
    assert_eq!(group["blocked"]["insufficient_members"]["required"], json!(2));
    assert_eq!(durable_frontier(&blocked), Some(0));

    // Heal the link: the replica's next beacon reaches the writer, the group meets its majority, and the
    // writer's readiness converges to ready with both members reporting an empty frontier.
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

    // Kill the replica and hold it down past the writer's dead window: its last beacon ages out, so the
    // writer counts only itself and the group falls below its majority.
    cluster.nodes_mut()[replica].kill();
    let blocked = await_readiness(&cluster.nodes()[writer], AGE_OUT, |document| {
        group_ready(document) == Some(false)
    });
    assert_eq!(
        blocked["group_readiness"]["blocked"]["insufficient_members"]["reporting"],
        json!(1),
    );

    // Restart the replica against the same store and port: it beacons its frontier afresh, the group
    // meets its majority again, and the writer's readiness recovers.
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

    // The empty group converges ready at frontier 0: writer and replica both report an applied serial of
    // zero, so the majority holds serial 0.
    await_readiness(writer, CONVERGE, |document| {
        group_ready(document) == Some(true)
            && durable_frontier(document) == Some(0)
            && writer_serial(document) == Some(0)
    });

    // Slow the link: the writer→replica direction is delayed, so the journal pages the replica applies
    // arrive late, but its beacon requests (replica→writer) still land - it keeps reporting a frontier.
    let proxy = proxied.proxy("replica-b").expect("the replica has a proxy");
    proxy.pause(LAG).expect("slow the replica's link to the writer");

    // Publish a wheel straight to the writer: its serial advances at once, but the slowed replica cannot
    // apply the new journal entry while the link is paused.
    let (code, _) = writer.publish().expect("the publish reaches the writer");
    assert_eq!(code, 200);

    // While the link is paused the lagging state is stable, so poll for a well-formed group document -     // the writer's advanced serial, both members reporting, a durable frontier present - rather than
    // trust a single snapshot that a transient auth or scheduling hiccup could leave a field out of. The
    // paused replica cannot advance, so the frontier read here holds until the link is restored.
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

    // Restore the link: the replica catches up, beacons the writer's serial, and the durable frontier
    // advances to it.
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

    // Publish and let the group settle: the writer holds a serial the replica has applied, so the
    // group's durable frontier reaches it and the whole quorum holds it.
    let (code, _) = cluster.nodes()[writer]
        .publish()
        .expect("the publish reaches the writer");
    assert_eq!(code, 200);
    let settled = await_readiness(&cluster.nodes()[writer], CONVERGE, |document| {
        group_ready(document) == Some(true)
            && writer_serial(document).is_some_and(|serial| serial > 0)
            && durable_frontier(document) == writer_serial(document)
    });
    let frontier = writer_serial(&settled).expect("the writer reports a serial");

    // Kill the writer, the sole source of serials, so no new write can be issued. A write to the
    // surviving replica is refused read-only, and the dead writer answers nothing.
    cluster.nodes_mut()[writer].kill();
    // The replica may still be detecting the writer's death, and a request in flight while the writer
    // dies can fail to land, so poll the publish until the replica answers its read-only refusal.
    let deadline = Instant::now() + CONVERGE;
    let (code, body) = loop {
        if let Some(refusal @ (503, _)) = cluster.nodes()[replica].publish() {
            break refusal;
        }
        assert!(
            Instant::now() < deadline,
            "the replica never refused a write read-only after the writer died",
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(code, 503);
    assert!(
        body.contains("read_only_replica"),
        "the replica refuses the write: {body}"
    );
    assert!(
        cluster.nodes()[writer].publish().is_err(),
        "the dead writer accepts no write",
    );

    // Restart the writer: the frontier the quorum already held survives the crash - the writer's serial
    // and the replica's applied serial both persisted - and no write slipped in while the writer was
    // down, so the serial has not moved.
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
