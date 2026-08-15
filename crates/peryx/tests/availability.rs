#![cfg(feature = "availability-e2e")]

mod analytics_replication;
mod harness;

use std::time::Duration;

use harness::{HarnessError, MemberSpec, Node, OwnershipControl, ProcessHarness, Role, Topology, Toxiproxy};

fn process_harness() -> ProcessHarness {
    ProcessHarness::new(env!("CARGO_BIN_EXE_peryx"))
}

#[test]
fn test_spawns_a_node_and_reports_ready() {
    let cluster = Topology::single()
        .with_process_harness(process_harness())
        .start()
        .expect("cluster starts");
    let node = &cluster.nodes()[0];
    assert!(node.is_ready(), "node should answer /+status");
    let (code, _) = node.readiness().expect("readiness reachable");
    assert!(code == 200 || code == 503, "unexpected readiness code {code}");
}

#[test]
fn test_observes_a_node_over_arbitrary_http() {
    let cluster = Topology::single()
        .with_process_harness(process_harness())
        .start()
        .expect("cluster starts");
    let node = &cluster.nodes()[0];
    let (code, body) = node.metrics().expect("metrics reachable");
    assert_eq!(code, 200);
    assert!(!body.is_empty(), "metrics body is empty");
    assert!(
        node.http_get("/+status").is_some(),
        "arbitrary http_get reaches the node"
    );
}

#[test]
fn test_detects_a_child_crash() {
    let mut cluster = Topology::single()
        .with_process_harness(process_harness())
        .start()
        .expect("cluster starts");
    let node = &mut cluster.nodes_mut()[0];
    assert!(node.is_running());

    node.kill();

    assert!(!node.is_running(), "harness should observe the killed child has exited");
    assert!(node.status().is_none(), "a dead node answers no HTTP");
}

#[test]
fn test_detects_a_port_collision() {
    // Holding the port prevents a false-positive connection to an unrelated listener.
    let held = std::net::TcpListener::bind("127.0.0.1:0").expect("hold a port");
    let taken = held.local_addr().unwrap().port();

    let error = process_harness()
        .spawn_on_port("collider", taken)
        .expect_err("a taken port must fail startup");

    assert!(
        matches!(error, HarnessError::ExitedEarly { .. } | HarnessError::NotReady { .. }),
        "{error}"
    );
}

#[test]
fn test_reports_a_readiness_failure_with_diagnostics() {
    let error = process_harness()
        .spawn_with_config(
            "broken",
            "[[index]]\nname = \"hosted\"\nhosted = true\n[[index.access_token]]\nname = \"t\"\nactions = [\"write\"]\n",
        )
        .expect_err("an invalid config must fail startup");

    let rendered = error.to_string();
    assert!(
        rendered.contains("log tail"),
        "diagnostics should include the log: {rendered}"
    );
}

#[test]
fn test_leaves_no_leaked_process() {
    let pid = {
        let mut cluster = Topology::single()
            .with_process_harness(process_harness())
            .start()
            .expect("cluster starts");
        cluster.nodes_mut()[0].pid()
    };
    assert!(
        !harness::process_alive(pid),
        "dropping the cluster must leave no peryx process (pid {pid})"
    );
}

#[test]
fn test_toxiproxy_partitions_and_heals_a_link() {
    let cluster = Topology::single()
        .with_process_harness(process_harness())
        .start()
        .expect("cluster starts");
    let node = &cluster.nodes()[0];
    let mut toxiproxy = Toxiproxy::start().expect("toxiproxy starts");
    let proxy = toxiproxy
        .proxy(&format!("127.0.0.1:{}", node.port()))
        .expect("create proxy");

    assert!(
        harness::reachable_through(proxy.endpoint()),
        "a healthy proxy forwards to the node"
    );

    proxy.partition().expect("partition the link");
    assert!(
        !harness::reachable_through(proxy.endpoint()),
        "a partitioned proxy drops the connection"
    );

    proxy.heal().expect("heal the link");
    assert!(
        harness::reachable_through(proxy.endpoint()),
        "a healed proxy forwards again"
    );
}

#[test]
fn test_detects_a_proxy_failure() {
    let mut toxiproxy = Toxiproxy::start().expect("toxiproxy starts");
    assert!(toxiproxy.control_is_up());

    toxiproxy.kill();

    assert!(
        !toxiproxy.control_is_up(),
        "harness should detect the dead proxy server"
    );
}

#[test]
fn test_captures_a_failure_artifact() {
    let cluster = Topology::single()
        .with_process_harness(process_harness())
        .start()
        .expect("cluster starts");
    let report = cluster.failure_report();
    let rendered = report.render();

    assert_eq!(report.nodes.len(), 1);
    assert!(rendered.contains("== node node-a =="), "{rendered}");
    assert!(
        rendered.contains("status:"),
        "artifact should include status: {rendered}"
    );
    assert!(rendered.contains("log:"), "artifact should include the log: {rendered}");
}

#[test]
fn test_validates_a_generated_ha_topology_config() {
    // Peer RPC is not mounted yet, so this checks accepted topology rather than quorum.
    let output = Topology::ha(
        "ownership",
        vec![
            MemberSpec::new("node-a", "east", Role::Writer),
            MemberSpec::new("node-b", "west", Role::Replica),
        ],
    )
    .with_process_harness(process_harness())
    .validate_config()
    .expect("peryx accepts the generated ha config");
    assert!(output.contains("configuration is valid"), "{output}");
    assert!(output.contains("availability: ha"), "{output}");
    assert!(output.contains("2 members"), "{output}");
}

#[test]
fn test_validates_a_generated_dc_topology_config() {
    let output = Topology::dc(
        "region",
        vec![
            MemberSpec::new("primary", "east", Role::Writer),
            MemberSpec::new("replica", "west", Role::Replica),
        ],
    )
    .with_process_harness(process_harness())
    .validate_config()
    .expect("peryx accepts the generated dc config");
    assert!(output.contains("configuration is valid"), "{output}");
}

#[test]
fn test_a_none_node_has_no_transfer_state() {
    let cluster = Topology::single()
        .with_process_harness(process_harness())
        .start()
        .expect("cluster starts");
    assert!(matches!(cluster.leader(), Ok(None)));
    assert!(matches!(
        cluster.await_authority_transfer("node-a", Duration::from_millis(200)),
        Err(HarnessError::NoTransfer { .. })
    ));
}

#[test]
fn test_dc_replica_metrics_exposes_the_replication_series() {
    // Offline writer identity provisioning lets the replica open its store read-only.
    let cluster = Topology::dc(
        "east",
        vec![
            MemberSpec::new("writer-a", "east-1", Role::Writer),
            MemberSpec::new("replica-b", "east-2", Role::Replica),
        ],
    )
    .with_process_harness(process_harness())
    .start()
    .expect("dc cluster starts");
    let replica = cluster.node("replica-b").expect("the replica is present");

    let (code, body) = replica.metrics().expect("metrics reachable");

    assert_eq!(code, 200);
    assert!(
        body.contains("peryx_ha_distributed_"),
        "a dc replica exports the replication series: {body}"
    );
}

#[test]
fn test_ha_replica_metrics_exposes_the_replication_series() {
    let cluster = Topology::ha(
        "global",
        vec![
            MemberSpec::new("writer-east", "east", Role::Writer),
            MemberSpec::new("replica-west", "west", Role::Replica),
        ],
    )
    .with_process_harness(process_harness())
    .start()
    .expect("ha cluster starts");
    let replica = cluster.node("replica-west").expect("the replica is present");

    let (code, body) = replica.metrics().expect("metrics reachable");

    assert_eq!(code, 200);
    assert!(
        body.contains("peryx_ha_distributed_"),
        "an ha replica exports the replication series: {body}"
    );
}

fn metric(body: &str, series: &str) -> Option<u64> {
    body.lines()
        .find_map(|line| line.strip_prefix(series).and_then(|rest| rest.trim().parse().ok()))
}

fn await_sync_error(node: &Node) {
    node.await_topology_signal(Duration::from_secs(30), |node| {
        let (status, body) = node.metrics().expect("metrics reachable");
        assert_eq!(status, 200, "metrics scrape failed: {body}\n{}", node.diagnostics());
        assert!(
            node.is_ready(),
            "the replica stopped serving during the metadata outage",
        );
        let errors = metric(&body, "peryx_ha_distributed_sync_errors_total ").expect("sync error metric is present");
        let caught_up = metric(&body, "peryx_ha_distributed_caught_up ").expect("caught-up metric is present");
        assert!(
            errors == 0 || caught_up == 0,
            "a replica with a metadata transport error is not caught up",
        );
        (
            (errors > 0).then_some(()),
            format!(
                "the replica never recorded a metadata transport loss; last metrics scrape:\n{body}\n{}",
                node.diagnostics(),
            ),
        )
    })
    .expect("the replica signals its transport loss");
}

fn await_caught_up(node: &Node) {
    node.await_topology_signal(Duration::from_secs(30), |node| {
        let (status, body) = node.metrics().expect("metrics reachable");
        assert_eq!(status, 200, "metrics scrape failed: {body}\n{}", node.diagnostics());
        let caught_up = metric(&body, "peryx_ha_distributed_caught_up ").expect("caught-up metric is present");
        (
            (caught_up == 1).then_some(()),
            format!(
                "the replica did not catch up; last metrics scrape:\n{body}\n{}",
                node.diagnostics(),
            ),
        )
    })
    .expect("the replica signals convergence");
}

#[test]
fn test_replica_recovers_metadata_after_a_writer_disconnect() {
    // The durable frontier prevents replay after a response is lost following apply.
    let mut cluster = Topology::dc(
        "east",
        vec![
            MemberSpec::new("writer-a", "east-1", Role::Writer),
            MemberSpec::new("replica-b", "east-2", Role::Replica),
        ],
    )
    .with_process_harness(process_harness())
    .start()
    .expect("dc cluster starts");
    let writer = cluster
        .nodes()
        .iter()
        .position(|node| node.identity() == "writer-a")
        .unwrap();
    let replica = cluster
        .nodes()
        .iter()
        .position(|node| node.identity() == "replica-b")
        .unwrap();

    await_caught_up(&cluster.nodes()[replica]);

    cluster.nodes_mut()[writer].kill();
    await_sync_error(&cluster.nodes()[replica]);

    cluster.nodes_mut()[writer]
        .restart()
        .expect("the writer restarts on its port");
    await_caught_up(&cluster.nodes()[replica]);
}
