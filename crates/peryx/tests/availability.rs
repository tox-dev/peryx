//! Self-tests for the multi-process availability harness: proof that it spawns real peryx binaries,
//! observes them, injects and heals network faults through Toxiproxy, and leaves nothing running.
//!
//! Gated behind the `availability-e2e` feature so the default `cargo test` and the coverage gate skip
//! them: they spawn processes and need the `toxiproxy-server` binary. CI runs them in a dedicated job.

#![cfg(feature = "availability-e2e")]

mod analytics_replication;
mod dc_group_readiness;
mod harness;
mod oci_failover;
mod pypi_failover;

use std::time::{Duration, Instant};

use harness::{HarnessError, MemberSpec, Node, OwnershipControl, Role, Topology, Toxiproxy};

#[test]
fn test_spawns_a_node_and_reports_ready() {
    let cluster = Topology::single().start().expect("cluster starts");
    let node = &cluster.nodes()[0];
    assert!(node.is_ready(), "node should answer /+status");
    let (code, _) = node.readiness().expect("readiness reachable");
    assert!(code == 200 || code == 503, "unexpected readiness code {code}");
}

#[test]
fn test_observes_a_node_over_arbitrary_http() {
    // The general http_get accessor (and the metrics convenience on top of it) lets a test reach any
    // read endpoint the harness has not named, which the observability test tiers build on.
    let cluster = Topology::single().start().expect("cluster starts");
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
    let mut cluster = Topology::single().start().expect("cluster starts");
    let node = &mut cluster.nodes_mut()[0];
    assert!(node.is_running());

    node.kill();

    assert!(!node.is_running(), "harness should observe the killed child has exited");
    assert!(node.status().is_none(), "a dead node answers no HTTP");
}

#[test]
fn test_detects_a_port_collision() {
    // Hold a port so the node cannot bind it. The harness must surface the failed startup rather than
    // attach to a foreign listener already on that port.
    let held = std::net::TcpListener::bind("127.0.0.1:0").expect("hold a port");
    let taken = held.local_addr().unwrap().port();

    let error = harness::spawn_on_port("collider", taken).expect_err("a taken port must fail startup");

    assert!(
        matches!(error, HarnessError::ExitedEarly { .. } | HarnessError::NotReady { .. }),
        "{error}"
    );
}

#[test]
fn test_reports_a_readiness_failure_with_diagnostics() {
    // A config naming an upload token with no secret is rejected at startup, so the node exits and the
    // harness surfaces the log rather than hanging.
    let error = harness::spawn_with_config(
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
        let mut cluster = Topology::single().start().expect("cluster starts");
        cluster.nodes_mut()[0].pid()
    };
    // The cluster dropped at the block's end; its Drop killed the process group. Poll for the exit
    // instead of sleeping a fixed span, so a slow reap under load waits for the process rather than
    // asserting against a race.
    let deadline = Instant::now() + Duration::from_secs(10);
    while harness::process_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !harness::process_alive(pid),
        "dropping the cluster must leave no peryx process (pid {pid})"
    );
}

#[test]
fn test_toxiproxy_partitions_and_heals_a_link() {
    let cluster = Topology::single().start().expect("cluster starts");
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
    let cluster = Topology::single().start().expect("cluster starts");
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
    // The embedded ownership node ([#498]) runs but a cluster cannot form yet — the inbound peer-RPC
    // router is unmounted, so bootstrap never reaches quorum. The reachable assertion is that the
    // topology builder generates config peryx accepts: a writer, a replica, the group, and the roster.
    let output = Topology::ha(
        "ownership",
        vec![
            MemberSpec::new("node-a", "east", Role::Writer),
            MemberSpec::new("node-b", "west", Role::Replica),
        ],
    )
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
    .validate_config()
    .expect("peryx accepts the generated dc config");
    assert!(output.contains("configuration is valid"), "{output}");
}

#[test]
fn test_a_none_node_exposes_leader_reads_but_no_ownership_write_api() {
    let cluster = Topology::single().start().expect("cluster starts");
    // A single `none` node runs no consensus group and mounts no availability status, so the leader read
    // is available but names no leader, and a transfer wait finds none before its deadline.
    assert!(matches!(cluster.leader(), Ok(None)));
    assert!(matches!(
        cluster.await_authority_transfer("node-a", Duration::from_millis(200)),
        Err(HarnessError::NoTransfer { .. })
    ));
    // The ownership write endpoint is still blocked on #540, the one control that stays unsupported.
    assert!(matches!(
        cluster.submit_ownership_write("x"),
        Err(HarnessError::Unsupported(_))
    ));
}

#[test]
fn test_dc_replica_metrics_exposes_the_replication_series() {
    // A real replica in a dc group follows the writer, so its scrape carries the replication series a
    // `none` node never exports. The harness bootstraps the replica's writer identity offline, which is
    // what lets a replica start read-only at all.
    let cluster = Topology::dc(
        "east",
        vec![
            MemberSpec::new("writer-a", "east-1", Role::Writer),
            MemberSpec::new("replica-b", "east-2", Role::Replica),
        ],
    )
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

fn metric(node: &Node, series: &str) -> u64 {
    let (_, body) = node.metrics().expect("metrics reachable");
    body.lines()
        .find_map(|line| line.strip_prefix(series).and_then(|rest| rest.trim().parse().ok()))
        .unwrap_or(0)
}

/// Poll until the replica records a metadata transport error, asserting it keeps serving read-only the
/// whole time, or fail after a generous deadline. Deterministic in outcome: it waits for the transition
/// rather than sleeping a fixed span and hoping.
fn await_sync_error(node: &Node) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        assert!(
            node.is_ready(),
            "the replica keeps serving read-only through the metadata outage"
        );
        if metric(node, "peryx_ha_distributed_sync_errors_total ") > 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("the replica never recorded a metadata transport loss after the writer died");
}

fn await_caught_up(node: &Node) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if metric(node, "peryx_ha_distributed_caught_up ") == 1 {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("the replica did not catch up: {:?}", node.metrics());
}

#[test]
fn test_replica_recovers_metadata_after_a_writer_disconnect() {
    // A real dc replica follows the writer over the bounded metadata transport. Killing the writer
    // disconnects the metadata link: the replica keeps serving read-only and its transport records the
    // loss (disconnect during a batch). Restarting the writer heals the link and the replica catches up
    // again, its durable frontier keeping the re-poll from replaying an already-applied change (response
    // loss after apply).
    let mut cluster = Topology::dc(
        "east",
        vec![
            MemberSpec::new("writer-a", "east-1", Role::Writer),
            MemberSpec::new("replica-b", "east-2", Role::Replica),
        ],
    )
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

#[test]
fn test_publishes_a_wheel_and_downloads_it_by_content_address() {
    // The publish and bytes-download helpers the multiprocess proofs build on: a real upload over the
    // legacy multipart API lands the fixture wheel on the node, and a GET of its content-addressed file
    // URL returns exactly the bytes that were published, byte for byte.
    let cluster = Topology::single().start().expect("cluster starts");
    let node = &cluster.nodes()[0];

    let (code, body) = node.publish().expect("publish reaches the node");
    assert_eq!((code, body.as_str()), (200, "upload accepted"));

    let (code, bytes) = node.download_wheel().expect("download reaches the node");
    assert_eq!(code, 200);
    assert_eq!(
        bytes,
        harness::WHEEL,
        "the download returns the published bytes unchanged"
    );
}

#[test]
fn test_with_oci_serves_the_distribution_v2_mutation_surface() {
    // The opt-in OCI seam: a node built with `with_oci` answers the distribution-spec `/v2/` handshake,
    // opens a blob upload session, commits a blob, and publishes a manifest — the mutating surface the
    // OCI-failover tier drives. The PyPI `hosted` index keeps working on the same node, so the seam is
    // additive.
    let cluster = Topology::single().with_oci().start().expect("cluster starts");
    let node = &cluster.nodes()[0];

    let (code, _) = node.oci_v2().expect("v2 reachable");
    assert_eq!(code, 200, "an OCI node answers the /v2/ version check");

    // Opening a blob upload session is a bare 202 Accepted.
    let (code, _) = node
        .oci_mutate(
            reqwest::Method::POST,
            &format!("/v2/{}/app/blobs/uploads/", harness::OCI_ROUTE),
        )
        .expect("upload session reachable");
    assert_eq!(code, 202, "opening a blob upload session is accepted");

    // A monolithic blob commit is 201 Created, and the blob then pulls back byte for byte.
    let blob = b"harness-oci-layer";
    let (code, digest) = node.oci_push_blob("app", blob).expect("blob push reaches the node");
    assert_eq!(code, 201, "a committed blob is created");
    let (code, bytes) = node.oci_pull_blob("app", &digest).expect("blob pull reaches the node");
    assert_eq!(
        (code, bytes.as_slice()),
        (200, blob.as_slice()),
        "the pulled blob is the pushed bytes",
    );

    // A manifest PUT under a tag is 201 Created, and the tag lists exactly once.
    let manifest = br#"{"schemaVersion":2}"#;
    let (code, _) = node
        .oci_put_manifest("app", "1.0", manifest, harness::OCI_MANIFEST_TYPE)
        .expect("manifest put reaches the node");
    assert_eq!(code, 201, "a published manifest is created");
    assert_eq!(
        node.oci_tags("app"),
        vec!["1.0".to_owned()],
        "the published tag lists once"
    );

    // The PyPI hosted index still serves on the same node: the OCI index is additive.
    let (code, body) = node.publish().expect("publish reaches the node");
    assert_eq!(
        (code, body.as_str()),
        (200, "upload accepted"),
        "PyPI publish still works"
    );
}

#[test]
fn test_without_oci_serves_no_v2_surface() {
    // The seam is opt-in: a topology that does not call `with_oci` mounts no OCI driver, so the `/v2/`
    // catch-all is absent and the version check resolves no index.
    let cluster = Topology::single().start().expect("cluster starts");
    let node = &cluster.nodes()[0];
    let (code, _) = node.oci_v2().expect("the node is reachable");
    assert_eq!(code, 404, "a node without with_oci serves no /v2/ API");
}

#[test]
fn test_a_node_serves_only_the_blobs_it_holds() {
    // The negative half of placing a blob on one node and not another: a node that never received the
    // upload has no local copy, so its content-addressed download is a 404 rather than a foreign hit.
    // The peer-serve read-through ([#923], gated on [#924]) is what later turns this miss into a fetch.
    let cluster = Topology::single().start().expect("cluster starts");
    let node = &cluster.nodes()[0];

    let (code, _) = node.download_wheel().expect("download reaches the node");
    assert_eq!(code, 404, "an unpublished blob has no local copy");
}
