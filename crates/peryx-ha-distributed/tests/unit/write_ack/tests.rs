use std::sync::Mutex;

use peryx_core::{NodeRole, TopologyMember};
use peryx_ha::{AuthorityEpoch, BlobWriteDurability as _, ByteAckDecision, CommittedBlob, WriteDurability};
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
                nodes: vec!["local".to_owned()]
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

#[tokio::test]
async fn local_peer_receipt_satisfies_majority() {
    let digest = digest();
    let durability = DistributedBlobDurability::new(
        TopologyConfig {
            members: vec![member("node-a", "east"), member("node-b", "east")],
            local_node: Some("node-a".to_owned()),
            ..TopologyConfig::default()
        },
        DurabilityPolicy::Majority,
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
async fn elapsed_byte_quorum_is_unknown() {
    let digest = digest();
    let observer = Arc::new(Observer::default());
    let durability = DistributedBlobDurability::new(
        TopologyConfig {
            members: vec![member("node-a", "east"), member("node-b", "east")],
            local_node: Some("node-a".to_owned()),
            ..TopologyConfig::default()
        },
        DurabilityPolicy::Everywhere,
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
                remaining: 1
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
