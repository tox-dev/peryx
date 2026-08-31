use std::sync::Mutex;

use peryx_core::{NodeRole, TopologyMember};
use peryx_ha::{AuthorityEpoch, ByteAckDecision, CommittedBlob, WriteDurability};
use peryx_storage::blob::{BlobDurability, Digest};
use peryx_storage::meta::{MetaError, MetaStore};

use super::*;
use crate::{LoopbackReceiptSource, LoopbackRemoteFrontierSource};

#[derive(Default)]
struct Observer(Mutex<Vec<(DcAck, ByteAckDecision)>>);

impl WriteAckObserver for Observer {
    fn record(&self, outcome: DcAck, byte_decision: &ByteAckDecision) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((outcome, byte_decision.clone()));
    }
}

const SIZE: u64 = 1;

fn digest() -> Digest {
    Digest::from_hex(&format!("{:064x}", 1)).unwrap()
}

fn member(node: &str, dc: &str) -> TopologyMember {
    TopologyMember {
        node: node.to_owned(),
        dc: dc.to_owned(),
        address: format!("http://{node}.example"),
        role: NodeRole::Writer,
    }
}

fn write(digest: &Digest) -> CommittedBlob<'_> {
    let directory = tempfile::tempdir().unwrap();
    let commit = MetaStore::open(directory.path().join("peryx.redb"))
        .unwrap()
        .commit_driver_txn_with_commit::<(), MetaError>(|txn| {
            txn.put("write", b"committed")?;
            Ok(((), vec![b"write".to_vec()]))
        })
        .unwrap()
        .journal
        .unwrap();
    CommittedBlob::new(
        digest,
        SIZE,
        "repository:alpha",
        AuthorityEpoch(2),
        Some(commit),
        BlobDurability::Filesystem,
    )
}

#[tokio::test]
async fn rosterless_local_write_is_durable_without_peer_work() {
    let observer = Arc::new(Observer::default());
    let durability = DistributedBlobDurability::new(
        TopologyConfig::default(),
        DurabilityPolicy::Local,
        Vec::new(),
        Vec::new(),
        Duration::ZERO,
        observer.clone(),
    );

    assert_eq!(
        durability.confirm(write(&digest())).await,
        WriteDurability::Confirmed {
            scope: BlobDurability::Filesystem
        }
    );
    assert_eq!(
        *observer.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner),
        [(
            DcAck::Durable {
                scope: BlobDurability::Filesystem
            },
            ByteAckDecision::Acknowledged {
                nodes: vec!["local".to_owned()],
                required: 1,
            }
        )]
    );
}

#[tokio::test]
async fn unjournaled_local_write_needs_no_metadata_acknowledgement() {
    let digest = digest();
    let durability = DistributedBlobDurability::new(
        TopologyConfig::default(),
        DurabilityPolicy::Local,
        Vec::new(),
        Vec::new(),
        Duration::ZERO,
        Arc::new(Observer::default()),
    );

    let outcome = durability
        .confirm(CommittedBlob::new(
            &digest,
            SIZE,
            "repository:alpha",
            AuthorityEpoch(2),
            None,
            BlobDurability::Filesystem,
        ))
        .await;

    assert_eq!(
        outcome,
        WriteDurability::Confirmed {
            scope: BlobDurability::Filesystem,
        },
    );
}

#[rstest::rstest]
#[case::majority(DurabilityPolicy::Majority)]
#[case::everywhere(DurabilityPolicy::Everywhere)]
#[tokio::test]
async fn local_peer_receipt_satisfies_two_member_quorum(#[case] policy: DurabilityPolicy) {
    let digest = digest();
    let durability = DistributedBlobDurability::new(
        TopologyConfig {
            members: vec![member("node-a", "east"), member("node-b", "east")],
            local_node: Some("node-a".to_owned()),
            ..TopologyConfig::default()
        },
        policy,
        vec![Arc::new(LoopbackReceiptSource::holding("node-b", digest.clone(), 1))],
        Vec::new(),
        Duration::from_millis(10),
        Arc::new(Observer::default()),
    );

    assert_eq!(
        durability.confirm(write(&digest)).await,
        WriteDurability::Confirmed {
            scope: BlobDurability::Filesystem
        }
    );
}

#[tokio::test]
async fn unresolved_topology_is_unavailable() {
    let observer = Arc::new(Observer::default());
    let durability = DistributedBlobDurability::new(
        TopologyConfig::default(),
        DurabilityPolicy::Majority,
        Vec::new(),
        Vec::new(),
        Duration::ZERO,
        observer.clone(),
    );

    assert_eq!(durability.confirm(write(&digest())).await, WriteDurability::Unavailable);
    assert!(
        observer
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_empty()
    );
}

#[rstest::rstest]
#[case::majority(DurabilityPolicy::Majority)]
#[case::everywhere(DurabilityPolicy::Everywhere)]
#[tokio::test]
async fn elapsed_two_member_quorum_is_unknown(#[case] policy: DurabilityPolicy) {
    let digest = digest();
    let observer = Arc::new(Observer::default());
    let durability = DistributedBlobDurability::new(
        TopologyConfig {
            members: vec![member("node-a", "east"), member("node-b", "east")],
            local_node: Some("node-a".to_owned()),
            ..TopologyConfig::default()
        },
        policy,
        vec![Arc::new(LoopbackReceiptSource::absent("node-b"))],
        Vec::new(),
        Duration::ZERO,
        observer.clone(),
    );

    assert_eq!(durability.confirm(write(&digest)).await, WriteDurability::Unavailable);
    assert_eq!(
        observer.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)[0],
        (
            DcAck::Unknown,
            ByteAckDecision::Pending {
                nodes: vec!["node-a".to_owned()],
                required: 2,
                remaining: 1,
            }
        )
    );
}

#[tokio::test]
async fn remote_frontier_satisfies_ha_metadata_durability() {
    let digest = digest();
    let committed = write(&digest);
    let durability = DistributedBlobDurability::new(
        TopologyConfig::default(),
        DurabilityPolicy::Local,
        Vec::new(),
        vec![Arc::new(LoopbackRemoteFrontierSource::reporting(
            "west",
            2,
            committed.commit().unwrap().serial(),
        ))],
        Duration::from_millis(10),
        Arc::new(Observer::default()),
    );

    assert_eq!(
        durability.confirm(committed).await,
        WriteDurability::Confirmed {
            scope: BlobDurability::Filesystem
        }
    );
}

#[tokio::test]
async fn elapsed_remote_frontier_is_unknown() {
    let digest = digest();
    let durability = DistributedBlobDurability::new(
        TopologyConfig::default(),
        DurabilityPolicy::Local,
        Vec::new(),
        vec![Arc::new(LoopbackRemoteFrontierSource::silent("west"))],
        Duration::ZERO,
        Arc::new(Observer::default()),
    );

    assert_eq!(durability.confirm(write(&digest)).await, WriteDurability::Unavailable);
}

/// One member per datacenter, the only roster shape `ha` accepts.
fn ha_topology() -> TopologyConfig {
    TopologyConfig {
        members: vec![
            member("node-a", "east"),
            member("node-b", "west"),
            member("node-c", "south"),
            member("node-d", "north"),
        ],
        local_node: Some("node-a".to_owned()),
        ..TopologyConfig::default()
    }
}

fn remotes(reporting: &[&str], silent: &[&str], serial: u64) -> Vec<Arc<dyn RemoteFrontierSource + Send + Sync>> {
    let reporting = reporting.iter().map(|dc| {
        Arc::new(LoopbackRemoteFrontierSource::reporting(*dc, 2, serial)) as Arc<dyn RemoteFrontierSource + Send + Sync>
    });
    let silent = silent
        .iter()
        .map(|dc| Arc::new(LoopbackRemoteFrontierSource::silent(*dc)) as Arc<dyn RemoteFrontierSource + Send + Sync>);
    reporting.chain(silent).collect()
}

#[rstest::rstest]
#[case::everywhere_short_of_the_quorum(DurabilityPolicy::Everywhere, &["west", "south"], &["north"], WriteDurability::Unavailable)]
#[case::everywhere_at_the_quorum(DurabilityPolicy::Everywhere, &["west", "south", "north"], &[], WriteDurability::Confirmed { scope: BlobDurability::Filesystem })]
#[case::majority_over_half(DurabilityPolicy::Majority, &["west", "south"], &["north"], WriteDurability::Confirmed { scope: BlobDurability::Filesystem })]
#[case::majority_below_half(DurabilityPolicy::Majority, &["west"], &["south", "north"], WriteDurability::Unavailable)]
#[case::local_from_one_remote(DurabilityPolicy::Local, &["west"], &["south", "north"], WriteDurability::Confirmed { scope: BlobDurability::Filesystem })]
#[tokio::test(start_paused = true)]
async fn write_ack_policy_sets_the_remote_datacenter_quorum(
    #[case] policy: DurabilityPolicy,
    #[case] reporting: &[&str],
    #[case] silent: &[&str],
    #[case] expected: WriteDurability,
) {
    let digest = digest();
    let committed = write(&digest);
    let serial = committed.commit().unwrap().serial();
    let durability = DistributedBlobDurability::new(
        ha_topology(),
        policy,
        Vec::new(),
        remotes(reporting, silent, serial),
        Duration::from_secs(5),
        Arc::new(Observer::default()),
    );

    assert_eq!(durability.confirm(committed).await, expected);
}

#[test]
fn a_pending_remote_quorum_reports_the_frontier_it_has_applied() {
    let durability = RemoteDurability::Pending {
        holders: vec!["west".to_owned()],
        durable_frontier: 70,
    };

    assert_eq!(
        remote_decision(&durability, 100),
        AckDecision::NotYetDurable {
            target: 100,
            durable_frontier: 70,
        }
    );
}

#[test]
fn a_met_remote_quorum_acknowledges_the_metadata_dimension() {
    let durability = RemoteDurability::Durable {
        holders: vec!["west".to_owned()],
    };

    assert_eq!(remote_decision(&durability, 100), AckDecision::Acknowledged);
}
