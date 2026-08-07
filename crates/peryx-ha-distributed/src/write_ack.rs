use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use peryx_core::TopologyConfig;
use peryx_ha::{
    DcAck, MetadataOperation, ReceiptSource, RemoteFrontierSource, WriteAckObserver, WriteAckRequest, WriteAcknowledger,
};

use crate::{
    AckDecision, DEFAULT_FRONTIER_POLL, DEFAULT_RECEIPT_POLL, Deadline, DurabilityPolicy, FilesystemAck, ReceiptAck,
    RemoteDurability, assess_remote_metadata_durability, gather_receipts, gather_remote_acks,
};

const STANDALONE_NODE: &str = "local";

pub struct DistributedWriteAcknowledger {
    topology: TopologyConfig,
    policy: DurabilityPolicy,
    receipt_sources: Vec<Arc<dyn ReceiptSource + Send + Sync>>,
    remote_sources: Vec<Arc<dyn RemoteFrontierSource + Send + Sync>>,
    budget: Duration,
    observer: Arc<dyn WriteAckObserver>,
}

impl DistributedWriteAcknowledger {
    #[must_use]
    pub const fn new(
        topology: TopologyConfig,
        policy: DurabilityPolicy,
        receipt_sources: Vec<Arc<dyn ReceiptSource + Send + Sync>>,
        remote_sources: Vec<Arc<dyn RemoteFrontierSource + Send + Sync>>,
        budget: Duration,
        observer: Arc<dyn WriteAckObserver>,
    ) -> Self {
        Self {
            topology,
            policy,
            receipt_sources,
            remote_sources,
            budget,
            observer,
        }
    }

    fn local_node(&self) -> String {
        self.topology
            .local_node
            .clone()
            .unwrap_or_else(|| STANDALONE_NODE.to_owned())
    }

    fn local_members(&self) -> BTreeSet<String> {
        let Some(datacenter) = self.topology.local_datacenter() else {
            return BTreeSet::from([self.local_node()]);
        };
        self.topology
            .members
            .iter()
            .filter(|member| member.dc == datacenter)
            .map(|member| member.node.clone())
            .collect()
    }

    async fn metadata_decision(&self, authority: &str, operation: MetadataOperation) -> (AckDecision, Deadline) {
        if self.remote_sources.is_empty() {
            return (AckDecision::Acknowledged, Deadline::Live);
        }
        let mut acknowledgements = Vec::new();
        let deadline = gather_remote_acks(
            &self.remote_sources,
            authority,
            &operation,
            &mut acknowledgements,
            self.budget,
            DEFAULT_FRONTIER_POLL,
        )
        .await;
        (
            remote_decision(
                &assess_remote_metadata_durability(&operation, &acknowledgements),
                operation.frontier,
            ),
            deadline,
        )
    }
}

#[async_trait]
impl WriteAcknowledger for DistributedWriteAcknowledger {
    async fn acknowledge(&self, request: WriteAckRequest<'_>) -> DcAck {
        let mut filesystem = FilesystemAck::new(request.digest.clone(), self.local_members(), self.policy);
        filesystem.record(ReceiptAck {
            node: self.local_node(),
            digest: request.digest.clone(),
        });
        let (byte_deadline, (metadata, metadata_deadline)) = tokio::join!(
            gather_receipts(
                &self.receipt_sources,
                request.digest,
                &mut filesystem,
                self.budget,
                DEFAULT_RECEIPT_POLL,
            ),
            self.metadata_decision(request.authority, request.operation),
        );
        let outcome = filesystem.decide(metadata, combined_deadline(byte_deadline, metadata_deadline));
        self.observer.record(outcome, &filesystem.byte_decision());
        outcome
    }
}

const fn remote_decision(durability: &RemoteDurability, frontier: u64) -> AckDecision {
    if durability.is_durable() {
        AckDecision::Acknowledged
    } else {
        AckDecision::NotYetDurable {
            target: frontier,
            durable_frontier: 0,
        }
    }
}

const fn combined_deadline(byte: Deadline, metadata: Deadline) -> Deadline {
    match (byte, metadata) {
        (Deadline::Live, Deadline::Live) => Deadline::Live,
        _ => Deadline::Expired,
    }
}

#[cfg(test)]
mod tests {
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
}
