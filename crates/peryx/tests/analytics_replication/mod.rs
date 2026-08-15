use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use peryx_events::metrics::{Metrics, Observation};
use peryx_storage::meta::MetaStore;
use serde_json::Value;

use super::harness::{ADMIN_PASSWORD, ADMIN_USER, MemberSpec, Node, ProcessHarness, Role, Topology};

const CONVERGE_TIMEOUT: Duration = Duration::from_secs(30);
const TOKEN: &str = "analytics-replication-token";
const PRODUCER: &str = "producer";
const REPLICA: &str = "replica";
const SEED_DOWNLOADS: u64 = 9;
const SEED_BYTES: u64 = 65_536;

#[test]
fn test_analytics_batches_replicate_and_survive_a_replica_restart() {
    let sealed_day = i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock follows the Unix epoch")
            .as_secs()
            / 86_400,
    )
    .expect("UTC day fits in i64")
        - 1;
    let mut cluster = Topology::dc(
        "analytics",
        vec![
            MemberSpec::new(PRODUCER, "dc-a", Role::Writer),
            MemberSpec::new(REPLICA, "dc-b", Role::Replica),
        ],
    )
    .with_process_harness(ProcessHarness::new(env!("CARGO_BIN_EXE_peryx")))
    .with_replication_token(TOKEN)
    .with_admin()
    .with_index_config("[[index]]\nname = \"hosted\"\nhosted = true\nvolatile = true")
    .start_with_data(|member, data| {
        if member.node == PRODUCER {
            seed_sealed_day(data, sealed_day);
        }
    })
    .expect("analytics cluster starts");
    let producer = cluster
        .nodes()
        .iter()
        .position(|node| node.identity() == PRODUCER)
        .expect("producer node");
    let replica = cluster
        .nodes()
        .iter()
        .position(|node| node.identity() == REPLICA)
        .expect("replica node");

    let expected = Completeness {
        downloads: SEED_DOWNLOADS,
        bytes: SEED_BYTES,
        accepted_epoch: Some(1),
        accepted_day: Some(sealed_day),
    };
    cluster.nodes()[replica]
        .await_log_signal(CONVERGE_TIMEOUT, "analytics batches applied")
        .expect("the replica persists an analytics batch");
    assert_eq!(completeness(&cluster.nodes()[replica]), Some(expected));

    cluster.nodes_mut()[producer].kill();
    cluster.nodes_mut()[replica]
        .restart()
        .expect("replica restarts with its durable store");
    assert_eq!(completeness(&cluster.nodes()[replica]), Some(expected));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Completeness {
    downloads: u64,
    bytes: u64,
    accepted_epoch: Option<u64>,
    accepted_day: Option<i64>,
}

fn completeness(node: &Node) -> Option<Completeness> {
    let (code, body) = node.http_get_as(ADMIN_USER, ADMIN_PASSWORD, "/+analytics/completeness")?;
    (code == 200).then_some(())?;
    let json: Value = serde_json::from_str(&body).ok()?;
    let totals = json.get("totals")?;
    let accepted = json
        .get("producers")?
        .as_array()?
        .iter()
        .find(|entry| entry.get("producer").and_then(Value::as_str) == Some(PRODUCER));
    Some(Completeness {
        downloads: totals.get("reads")?.as_u64()?,
        bytes: totals.get("bytes")?.as_u64()?,
        accepted_epoch: accepted
            .and_then(|entry| entry.get("accepted_epoch"))
            .and_then(Value::as_u64),
        accepted_day: accepted
            .and_then(|entry| entry.get("accepted_day"))
            .and_then(Value::as_i64),
    })
}

fn seed_sealed_day(data: &Path, sealed_day: i64) {
    let store = MetaStore::open(data.join("peryx.redb")).expect("open producer store");
    let metrics = Metrics::start_durable(store.analytics(), None, Arc::new(move || sealed_day * 86_400))
        .expect("start durable metrics");
    for read in 0..SEED_DOWNLOADS {
        metrics.record(Observation::Read {
            repository: "hosted".to_owned(),
            resource: "resource-a".to_owned(),
            artifact: "resource-a-revision-a.bin".to_owned(),
            group: Some("revision-a".to_owned()),
            source: None,
            bytes: SEED_BYTES / SEED_DOWNLOADS + u64::from(read < SEED_BYTES % SEED_DOWNLOADS),
        });
    }
    metrics.shutdown().expect("seed sealed day");
}
