//! `PyPI` mutations through a home-datacenter failover, over a live multi-process `ha` group.
//!
//! These scenarios drive real `PyPI` mutating operations against a spawned `ha` cluster and assert the
//! client observes one coherent terminal result across a home-datacenter failure: an upload to the home
//! writer is accepted as a retry-safe operation and its bytes, digest, serial, metadata, and operation
//! identity survive the home datacenter dying and recovering; a retry after recovery resolves to the same
//! operation rather than a duplicate; and a read-only replica refuses every mutation before and after the
//! home fails, so no write is ever silently accepted off the home authority.
//!
//! They observe only the public HTTP surface and the admin-gated availability resources, the way an
//! operator would, and they wait on asserted state (readiness, a served artifact, an agreed leader) rather
//! than sleeping a fixed span, so the outcome is deterministic. A live `ha` upload stays
//! `202 durability pending, retry-safe` here because no remote datacenter acknowledges the operation's
//! cross-DC durability in this harness; the terminal client contract under test is that retry-safety and
//! its home-local durability hold across the failover, not that the operation reaches a durable `200`.

use std::time::{Duration, Instant};

use reqwest::StatusCode;
use serde_json::Value;

use crate::harness::{ADMIN_PASSWORD, ADMIN_USER, Cluster, MemberSpec, Node, Role, Topology, UPLOAD_TOKEN};

/// The read-only replica's flat rejection of any mutation, returned before routing reaches the `PyPI`
/// driver, so an upload, a yank, and a delete all fail identically off the home authority.
const READ_ONLY_BODY: &str = r#"{"error":"read_only_replica","message":"this replica does not accept mutations"}"#;

/// A home-datacenter `ha` group: one writer at `east` and a read replica in each of two other
/// datacenters, so a quorum of two survives the writer's death and the group keeps a committed leader
/// through the failover.
fn home_dc_group() -> Cluster {
    Topology::ha(
        "global",
        vec![
            MemberSpec::new("writer-east", "east", Role::Writer),
            MemberSpec::new("replica-west", "west", Role::Replica),
            MemberSpec::new("replica-south", "south", Role::Replica),
        ],
    )
    .with_admin()
    .with_write_ack_deadline(1)
    .start()
    .expect("the ha group starts")
}

/// A `method` request with the upload credential to `path` on a node's public port, for a raw yank or
/// delete the harness has no typed helper for. A read-only replica rejects it before auth, so the
/// credential only matters where a writer would honor it.
fn mutate(node: &Node, method: reqwest::Method, path: &str) -> (u16, String) {
    let response = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("build client")
        .request(method, format!("http://127.0.0.1:{}{path}", node.port()))
        .basic_auth("__token__", Some(UPLOAD_TOKEN))
        .send()
        .expect("mutation reaches the node");
    let code = response.status().as_u16();
    (code, response.text().unwrap_or_default())
}

/// The writer's committed metadata serial, read as the topology frontier under the administrator gate.
fn writer_serial(node: &Node) -> Option<u64> {
    let (200, body) = node.http_get_as(ADMIN_USER, ADMIN_PASSWORD, "/+availability/topology")? else {
        return None;
    };
    serde_json::from_str::<Value>(&body)
        .ok()?
        .get("local")?
        .get("frontier")?
        .as_u64()
}

/// The ledger's operation rows on a node, admin-gated, so a test can name the single operation an upload
/// created and prove a retry did not fork a second one.
fn operations(node: &Node) -> Vec<String> {
    let Some((200, body)) = node.http_get_as(ADMIN_USER, ADMIN_PASSWORD, "/+availability/operations") else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&body) else {
        return Vec::new();
    };
    value
        .get("rows")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter_map(|row| row.get("operation").and_then(Value::as_str).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// Wait until a quorum of nodes agrees on the same committed leader over three voters, the signal that the
/// `ha` group has formed and can drive its durability plane. Polls state rather than sleeping a fixed span.
fn await_group_leader(cluster: &Cluster) -> String {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Some(leader) = quorum_leader(cluster) {
            return leader;
        }
        assert!(
            Instant::now() < deadline,
            "the ha group did not agree on a leader:\n{}",
            cluster.failure_report().render(),
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Wait until a quorum agrees on a leader other than `old`, the signal that authority failed over off the
/// killed home datacenter.
fn await_leader_change(cluster: &Cluster, old: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Some(leader) = quorum_leader(cluster).filter(|leader| leader != old) {
            return leader;
        }
        assert!(
            Instant::now() < deadline,
            "authority did not leave {old}:\n{}",
            cluster.failure_report().render(),
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The leader a majority of nodes report under a committed three-voter membership, or `None` until they
/// concur. A single node's view lags the group by a heartbeat, so agreement across a quorum is the settled
/// signal that never flaps on a transient read.
fn quorum_leader(cluster: &Cluster) -> Option<String> {
    let mut tally: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for node in cluster.nodes() {
        let Some((200, body)) = node.control_get_as(ADMIN_USER, ADMIN_PASSWORD, "/availability/v1/status") else {
            continue;
        };
        let Ok(status) = serde_json::from_str::<Value>(&body) else {
            continue;
        };
        let Some(consensus) = status.get("consensus") else {
            continue;
        };
        let three_voters = consensus
            .get("voters")
            .and_then(Value::as_array)
            .is_some_and(|voters| voters.len() == 3);
        if let Some(leader) = consensus.get("leader").and_then(Value::as_str)
            && three_voters
        {
            *tally.entry(leader.to_owned()).or_default() += 1;
        }
    }
    tally
        .into_iter()
        .find(|(_, count)| *count >= 2)
        .map(|(leader, _)| leader)
}

/// Wait until the node answers a content-addressed download of the fixture wheel with exactly its bytes,
/// the signal that the home store came back intact after a restart. Polls the served artifact, so a slow
/// reopen waits rather than racing.
fn await_wheel_served(node: &Node) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some((200, bytes)) = node.download_wheel()
            && bytes == crate::harness::WHEEL
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the home node did not serve the wheel after recovery: {:?}",
            node.status(),
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Wait until the node stops answering HTTP, so a test acts on the home's death rather than racing the
/// kill.
fn await_down(node: &Node) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while node.status().is_some() {
        assert!(Instant::now() < deadline, "the killed node kept answering HTTP");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn test_home_dc_upload_is_retry_safe_and_survives_a_home_failure() {
    // Publish the fixture wheel to the home writer, then kill and restart the home datacenter and retry the
    // identical upload. The client must observe one terminal result across the failure: the artifact's
    // bytes, digest, serial, metadata, and operation identity survive the home dying, and the retry
    // resolves to the same operation rather than a duplicate or a second serial.
    let mut cluster = home_dc_group();
    await_group_leader(&cluster);
    let writer = cluster.node("writer-east").expect("the home writer is present");

    let (code, body) = writer.publish().expect("publish reaches the home writer");
    assert!(
        (code == 200 || code == 202) && body.starts_with("upload accepted"),
        "the home upload is accepted as retry-safe: {code} {body:?}",
    );

    let (code, bytes) = writer.download_wheel().expect("download reaches the home writer");
    assert_eq!(
        (code, bytes.as_slice()),
        (200, crate::harness::WHEEL),
        "the home serves the published bytes"
    );

    let serial = writer_serial(writer).expect("the writer reports a committed serial");
    let operation = {
        let ops = operations(writer);
        assert_eq!(ops.len(), 1, "the upload created exactly one operation: {ops:?}");
        assert!(
            ops[0].contains(&crate::harness::wheel_digest()),
            "the operation is keyed by the artifact digest: {}",
            ops[0],
        );
        ops[0].clone()
    };
    assert!(
        detail_lists_wheel_once(writer),
        "the home lists the release exactly once before the failure"
    );

    // The home datacenter fails and comes back on its own store. Two survivors keep a quorum, so the group
    // stays available through the outage; the returning home reopens the same durable store.
    let home = cluster
        .nodes_mut()
        .iter_mut()
        .find(|node| node.identity() == "writer-east")
        .unwrap();
    home.kill();
    await_down(home);
    home.restart().expect("the home datacenter restarts on its store");
    let writer = cluster.node("writer-east").expect("the home writer is back");
    await_wheel_served(writer);

    // Nothing was lost or corrupted: the same bytes, the same committed serial, the same single operation.
    let (code, bytes) = writer.download_wheel().expect("download reaches the recovered home");
    assert_eq!(
        (code, bytes.as_slice()),
        (200, crate::harness::WHEEL),
        "the recovered home serves the identical bytes",
    );
    assert_eq!(
        writer_serial(writer),
        Some(serial),
        "the committed serial is unchanged across the failure",
    );
    assert!(
        detail_lists_wheel_once(writer),
        "the recovered home still lists the release exactly once"
    );

    // The retry after recovery resolves to the same operation, not a duplicate or a second serial.
    let (code, body) = writer.publish().expect("the retry reaches the recovered home");
    assert!(
        (code == 200 || code == 202) && body.starts_with("upload accepted"),
        "the retry is accepted as the same terminal result: {code} {body:?}",
    );
    assert_eq!(
        operations(writer),
        vec![operation],
        "the retry replays the original operation rather than forking a new one",
    );
    assert_eq!(
        writer_serial(writer),
        Some(serial),
        "the idempotent retry advances no serial",
    );
    assert!(
        detail_lists_wheel_once(writer),
        "the retry leaves exactly one release, no duplicate"
    );
}

#[test]
fn test_ha_replica_rejects_pypi_mutations_before_and_after_a_home_failure() {
    // A read replica in another datacenter must refuse every PyPI mutation, so no write is ever accepted off
    // the home authority. This holds through the home failing: killing the home writer never promotes a
    // replica to accept writes, which is what keeps the group from forking two homes.
    let mut cluster = home_dc_group();
    await_group_leader(&cluster);
    let replica = cluster.node("replica-west").expect("the replica is present");

    assert_replica_refuses_mutations(replica);

    // The home datacenter dies. Two survivors elect a new leader, but the replica's read-only posture is
    // fixed at startup and a failover never flips it, so it still refuses every mutation.
    let home = cluster
        .nodes_mut()
        .iter_mut()
        .find(|node| node.identity() == "writer-east")
        .unwrap();
    home.kill();
    await_down(home);
    await_leader_change(&cluster, "east");

    let replica = cluster.node("replica-west").expect("the replica survives");
    assert!(
        replica.is_ready(),
        "the replica keeps serving reads after the home fails",
    );
    assert_replica_refuses_mutations(replica);
}

/// Assert the replica refuses an upload, a yank, and a delete with the same flat read-only rejection, the
/// three `PyPI` mutation verbs a client drives through `POST`, `PUT`, and `DELETE`.
fn assert_replica_refuses_mutations(replica: &Node) {
    let (code, body) = replica.publish().expect("upload reaches the replica");
    assert_eq!(
        (code, body.as_str()),
        (StatusCode::SERVICE_UNAVAILABLE.as_u16(), READ_ONLY_BODY),
        "the replica refuses an upload",
    );
    for (method, path) in [
        (reqwest::Method::PUT, "/hosted/veloxdemo/1.0.0/yank"),
        (reqwest::Method::DELETE, "/hosted/veloxdemo/1.0.0/"),
    ] {
        let (code, body) = mutate(replica, method.clone(), path);
        assert_eq!(
            (code, body.as_str()),
            (StatusCode::SERVICE_UNAVAILABLE.as_u16(), READ_ONLY_BODY),
            "the replica refuses a {method} {path}",
        );
    }
}

/// Whether the hosted index lists the fixture project's one file exactly once, the metadata proof that a
/// retry left no duplicate release. Reads the PEP 691 JSON detail on the public port and counts the files
/// array, so it holds regardless of how the HTML representation repeats a filename.
fn detail_lists_wheel_once(node: &Node) -> bool {
    let response = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("build client")
        .get(format!("http://127.0.0.1:{}/hosted/simple/veloxdemo/", node.port()))
        .header(reqwest::header::ACCEPT, "application/vnd.pypi.simple.v1+json")
        .send()
        .expect("detail reaches the node");
    if response.status() != StatusCode::OK {
        return false;
    }
    let Ok(detail) = response.json::<Value>() else {
        return false;
    };
    detail.get("files").and_then(Value::as_array).is_some_and(|files| {
        files.len() == 1 && files[0].get("filename").and_then(Value::as_str) == Some(crate::harness::WHEEL_FILENAME)
    })
}
