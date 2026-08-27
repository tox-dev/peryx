use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use peryx_core::TopologyConfig;
use peryx_ha::{
    BlobWriteDurability, CommittedBlob, DcAck, MetadataOperation, ReceiptSource, RemoteFrontierSource,
    WriteAckObserver, WriteDurability,
};

use crate::{
    AckDecision, DEFAULT_FRONTIER_POLL, DEFAULT_RECEIPT_POLL, Deadline, DurabilityPolicy, FilesystemAck, ReceiptAck,
    RemoteDurability, assess_remote_metadata_durability, gather_receipts, gather_remote_acks,
};

const STANDALONE_NODE: &str = "local";

pub struct DistributedBlobDurability {
    topology: TopologyConfig,
    policy: DurabilityPolicy,
    receipt_sources: Vec<Arc<dyn ReceiptSource + Send + Sync>>,
    remote_sources: Vec<Arc<dyn RemoteFrontierSource + Send + Sync>>,
    budget: Duration,
    observer: Arc<dyn WriteAckObserver>,
}

impl DistributedBlobDurability {
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

    fn local_members(&self) -> Option<BTreeSet<String>> {
        if self.policy == DurabilityPolicy::Local {
            return Some(BTreeSet::from([self.local_node()]));
        }
        let datacenter = self.topology.local_datacenter()?;
        Some(
            self.topology
                .members
                .iter()
                .filter(|member| member.dc == datacenter)
                .map(|member| member.node.clone())
                .collect(),
        )
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
impl BlobWriteDurability for DistributedBlobDurability {
    async fn confirm(&self, write: CommittedBlob<'_>) -> WriteDurability {
        let Some(local_members) = self.local_members() else {
            return WriteDurability::Unavailable;
        };
        let mut filesystem = FilesystemAck::new(write.digest().clone(), local_members, self.policy);
        filesystem.record(ReceiptAck {
            node: self.local_node(),
            digest: write.digest().clone(),
        });
        let metadata = async {
            let Some(commit) = write.commit() else {
                return (AckDecision::Acknowledged, Deadline::Live);
            };
            self.metadata_decision(
                write.authority(),
                MetadataOperation {
                    epoch: write.epoch().0,
                    frontier: commit.serial(),
                },
            )
            .await
        };
        let (byte_deadline, (metadata, metadata_deadline)) = tokio::join!(
            gather_receipts(
                &self.receipt_sources,
                write.digest(),
                &mut filesystem,
                self.budget,
                DEFAULT_RECEIPT_POLL,
            ),
            metadata,
        );
        let outcome = filesystem.decide(metadata, combined_deadline(byte_deadline, metadata_deadline));
        self.observer.record(outcome, &filesystem.byte_decision());
        match outcome {
            DcAck::Durable { scope } => WriteDurability::Confirmed { scope },
            DcAck::Pending => WriteDurability::Pending,
            DcAck::Unknown => WriteDurability::Unavailable,
        }
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
#[path = "../tests/unit/write_ack/tests.rs"]
mod tests;
