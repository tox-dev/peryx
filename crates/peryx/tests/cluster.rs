#![cfg(feature = "availability-e2e")]

mod harness;

use std::collections::HashMap;
use std::time::Duration;

use harness::{ADMIN_PASSWORD, ADMIN_USER, Cluster, MemberSpec, ProcessHarness, Role, Topology, cargo_binary};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum FixtureNode {
    Writer,
    WestReplica,
    SouthReplica,
}

impl FixtureNode {
    const fn identity(self) -> &'static str {
        match self {
            Self::Writer => "node-a",
            Self::WestReplica => "node-b",
            Self::SouthReplica => "node-c",
        }
    }

    const fn datacenter(self) -> &'static str {
        match self {
            Self::Writer => "east",
            Self::WestReplica => "west",
            Self::SouthReplica => "south",
        }
    }

    fn from_datacenter(value: &str) -> Option<Self> {
        [Self::Writer, Self::WestReplica, Self::SouthReplica]
            .into_iter()
            .find(|node| node.datacenter() == value)
    }
}

#[test]
fn test_a_three_node_ha_cluster_forms_and_reports_its_leader() {
    let cluster = Topology::ha(
        "ownership",
        vec![
            MemberSpec::new("node-a", "east", Role::Writer),
            MemberSpec::new("node-b", "west", Role::Replica),
            MemberSpec::new("node-c", "south", Role::Replica),
        ],
    )
    .with_process_harness(ProcessHarness::new(cargo_binary("peryx")))
    .with_admin()
    .start()
    .expect("the three-node ha cluster starts");

    let (_, consensus) = await_quorum_leader(&cluster);
    let voters = consensus["voters"].as_array().expect("voters is an array").len();
    assert_eq!(
        voters, 3,
        "the committed membership holds all three voters: {consensus}"
    );
}

#[test]
fn test_killing_the_home_leader_fails_authority_over_to_a_survivor() {
    let mut cluster = Topology::ha(
        "ownership",
        vec![
            MemberSpec::new("node-a", "east", Role::Writer),
            MemberSpec::new("node-b", "west", Role::Replica),
            MemberSpec::new("node-c", "south", Role::Replica),
        ],
    )
    .with_process_harness(ProcessHarness::new(cargo_binary("peryx")))
    .with_admin()
    .start()
    .expect("the three-node ha cluster starts");

    let (home, _) = await_quorum_leader(&cluster);
    let node = cluster
        .nodes_mut()
        .iter_mut()
        .find(|node| node.identity() == home.identity())
        .expect("the leader datacenter runs one of the nodes");
    node.kill();

    assert_ne!(await_leader_change(&cluster, home), home);
}

fn await_leader_change(cluster: &Cluster, old: FixtureNode) -> FixtureNode {
    cluster
        .await_topology_signal(Duration::from_secs(90), |cluster| {
            let leader = quorum_leader(cluster)
                .map(|(leader, _)| leader)
                .filter(|leader| *leader != old);
            (
                leader,
                format!(
                    "authority did not leave {} within the deadline:\n{}",
                    old.datacenter(),
                    cluster.failure_report().render(),
                ),
            )
        })
        .expect("a surviving node signals the leader change")
}

fn await_quorum_leader(cluster: &Cluster) -> (FixtureNode, Value) {
    cluster
        .await_topology_signal(Duration::from_secs(90), |cluster| {
            (
                quorum_leader(cluster),
                format!(
                    "the ha group did not agree on a leader within the deadline:\n{}",
                    cluster.failure_report().render(),
                ),
            )
        })
        .expect("a node signals quorum agreement")
}

fn quorum_leader(cluster: &Cluster) -> Option<(FixtureNode, Value)> {
    let mut agreed: HashMap<FixtureNode, (usize, Value)> = HashMap::new();
    for (leader, block) in cluster
        .nodes()
        .iter()
        .filter_map(|node| node.control_get_as(ADMIN_USER, ADMIN_PASSWORD, "/availability/v1/status"))
        .filter(|(status, _)| *status == 200)
        .filter_map(|(_, body)| consensus_vote(&body))
    {
        let entry = agreed.entry(leader).or_insert((0, block));
        entry.0 += 1;
    }
    agreed
        .into_iter()
        .find(|(_, (count, _))| *count >= 2)
        .map(|(leader, (_, block))| (leader, block))
}

fn consensus_vote(body: &str) -> Option<(FixtureNode, Value)> {
    let status = serde_json::from_str::<Value>(body).ok()?;
    let block = status.get("consensus")?;
    let leader = block
        .get("leader")
        .and_then(Value::as_str)
        .and_then(FixtureNode::from_datacenter)?;
    (block.get("voters").and_then(Value::as_array).map(Vec::len) == Some(3)).then(|| (leader, block.clone()))
}

#[test]
fn test_fixture_nodes_map_identity_and_datacenter() {
    assert_eq!(FixtureNode::WestReplica.identity(), "node-b");
    assert_eq!(FixtureNode::SouthReplica.identity(), "node-c");
    assert_eq!(FixtureNode::from_datacenter("west"), Some(FixtureNode::WestReplica));
    assert_eq!(FixtureNode::from_datacenter("south"), Some(FixtureNode::SouthReplica));
    assert_eq!(FixtureNode::from_datacenter("unknown"), None);
}

#[test]
fn test_consensus_vote_requires_valid_leader_and_three_voters() {
    for body in [
        "invalid",
        "{}",
        r#"{"consensus":{"leader":"unknown","voters":[1,2,3]}}"#,
        r#"{"consensus":{"leader":"east","voters":[1,2]}}"#,
    ] {
        assert_eq!(consensus_vote(body), None);
    }

    let body = r#"{"consensus":{"leader":"east","voters":[1,2,3]}}"#;
    let (leader, block) = consensus_vote(body).expect("valid consensus vote");
    assert_eq!(leader, FixtureNode::Writer);
    assert_eq!(block["voters"].as_array().map(Vec::len), Some(3));
}
