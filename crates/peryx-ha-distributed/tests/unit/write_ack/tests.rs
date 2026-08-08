use std::sync::Mutex;

use peryx_core::{NodeRole, TopologyMember};
use peryx_ha::{ByteAckDecision, WriteAcknowledger as _};
use peryx_storage::blob::{BlobDurability, Digest};

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

fn request(digest: &Digest) -> WriteAckRequest<'_> {
    WriteAckRequest {
        digest,
        authority: "repository:alpha",
        operation: MetadataOperation { epoch: 2, frontier: 7 },
    }
}

#[tokio::test]
async fn rosterless_local_write_is_durable_without_peer_work() {
    let observer = Arc::new(Observer::default());
    let acknowledger = DistributedWriteAcknowledger::new(
        TopologyConfig::default(),
        DurabilityPolicy::Local,
        Vec::new(),
        Vec::new(),
        Duration::ZERO,
        observer.clone(),
    );

    assert_eq!(
        acknowledger.acknowledge(request(&digest())).await,
        DcAck::Durable {
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
async fn local_peer_receipt_satisfies_majority() {
    let digest = digest();
    let acknowledger = DistributedWriteAcknowledger::new(
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
        acknowledger.acknowledge(request(&digest)).await,
        DcAck::Durable {
            scope: BlobDurability::Filesystem
        }
    );
}

#[tokio::test]
async fn elapsed_byte_quorum_is_unknown() {
    let digest = digest();
    let observer = Arc::new(Observer::default());
    let acknowledger = DistributedWriteAcknowledger::new(
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

    assert_eq!(acknowledger.acknowledge(request(&digest)).await, DcAck::Unknown);
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
    let acknowledger = DistributedWriteAcknowledger::new(
        TopologyConfig::default(),
        DurabilityPolicy::Local,
        Vec::new(),
        vec![Arc::new(LoopbackRemoteFrontierSource::reporting("west", 2, 7))],
        Duration::from_millis(10),
        Arc::new(Observer::default()),
    );

    assert_eq!(
        acknowledger.acknowledge(request(&digest)).await,
        DcAck::Durable {
            scope: BlobDurability::Filesystem
        }
    );
}

#[tokio::test]
async fn elapsed_remote_frontier_is_unknown() {
    let digest = digest();
    let acknowledger = DistributedWriteAcknowledger::new(
        TopologyConfig::default(),
        DurabilityPolicy::Local,
        Vec::new(),
        vec![Arc::new(LoopbackRemoteFrontierSource::silent("west"))],
        Duration::ZERO,
        Arc::new(Observer::default()),
    );

    assert_eq!(acknowledger.acknowledge(request(&digest)).await, DcAck::Unknown);
}
