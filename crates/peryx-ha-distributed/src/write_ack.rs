use std::collections::BTreeSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use peryx_core::TopologyConfig;
use peryx_ha::{
    BlobWriteDurability, ByteEvidence, CommittedBlob, CommittedMetadata, DcAck, MetadataAckObservation,
    MetadataEvidence, MetadataOperation, MetadataWriteDurability, ReceiptRequest, ReceiptSource, RemoteFrontierSource,
    WriteAckDecision, WriteAckObserver, WriteDurability, WriteEvidence,
};

use crate::{
    AckDecision, DEFAULT_FRONTIER_POLL, DEFAULT_RECEIPT_POLL, Deadline, DurabilityPolicy, FilesystemAck, ReceiptAck,
    RemoteDurability, assess_remote_metadata_durability, decide_dc_ack, gather_receipts, gather_remote_acks,
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

    /// Counts this node's copy, then asks the datacenter's other members for theirs while the metadata
    /// dimension resolves.
    async fn gather_node_copies(
        &self,
        write: &CommittedBlob<'_>,
        local_members: BTreeSet<String>,
        metadata: impl Future<Output = (AckDecision, Deadline)>,
    ) -> (ByteEvidence, Deadline, (AckDecision, Deadline)) {
        let mut filesystem = FilesystemAck::new(write.digest().clone(), local_members, self.policy);
        filesystem.record(ReceiptAck {
            node: self.local_node(),
            digest: write.digest().clone(),
        });
        let (byte_deadline, metadata) = tokio::join!(
            gather_receipts(
                &self.receipt_sources,
                ReceiptRequest {
                    digest: write.digest(),
                    size: write.size(),
                },
                &mut filesystem,
                self.budget,
                DEFAULT_RECEIPT_POLL,
            ),
            metadata,
        );
        (filesystem.evidence(), byte_deadline, metadata)
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
            self.policy,
            self.budget,
            DEFAULT_FRONTIER_POLL,
        )
        .await;
        let durability =
            assess_remote_metadata_durability(&operation, &acknowledgements, self.remote_sources.len(), self.policy);
        (remote_decision(&durability, operation.frontier), deadline)
    }
}

#[async_trait]
impl BlobWriteDurability for DistributedBlobDurability {
    async fn confirm(&self, write: CommittedBlob<'_>) -> WriteDurability {
        let Some(local_members) = self.local_members() else {
            return WriteDurability::Unavailable;
        };
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
        let (evidence, byte_deadline, (metadata, metadata_deadline)) = match write.evidence() {
            WriteEvidence::NodeLocal => self.gather_node_copies(&write, local_members, metadata).await,
            // The store holds the one copy these nodes share, so polling them for receipts would count
            // that object once per reader rather than finding a second copy.
            evidence => (
                ByteEvidence::ObjectStore {
                    acknowledged: evidence == WriteEvidence::ObjectStoreVerified,
                },
                Deadline::Live,
                metadata.await,
            ),
        };
        let outcome = decide_dc_ack(metadata, &evidence, combined_deadline(byte_deadline, metadata_deadline));
        self.observer.record(outcome, &evidence);
        match outcome {
            DcAck::Durable { scope } => WriteDurability::Confirmed { scope },
            DcAck::Pending => WriteDurability::Pending,
            DcAck::Unknown => WriteDurability::Unavailable,
        }
    }
}

#[async_trait]
impl MetadataWriteDurability for DistributedBlobDurability {
    async fn confirm_metadata(&self, write: CommittedMetadata<'_>) -> WriteDurability {
        let started = tokio::time::Instant::now();
        let (metadata, deadline) = self
            .metadata_decision(
                write.authority(),
                MetadataOperation {
                    epoch: write.epoch().0,
                    frontier: write.commit().serial(),
                },
            )
            .await;
        let (durability, decision) = if metadata.is_acknowledged() {
            (
                WriteDurability::Confirmed {
                    scope: peryx_core::BlobDurability::Filesystem,
                },
                WriteAckDecision::Confirmed,
            )
        } else {
            (WriteDurability::Unavailable, WriteAckDecision::Unavailable)
        };
        self.observer.record_metadata(MetadataAckObservation {
            policy: self.policy,
            evidence: MetadataEvidence::JournalFrontier,
            waited: started.elapsed(),
            timed_out: deadline == Deadline::Expired,
            decision,
        });
        durability
    }
}

const fn remote_decision(durability: &RemoteDurability, frontier: u64) -> AckDecision {
    match durability {
        RemoteDurability::Durable { .. } => AckDecision::Acknowledged,
        RemoteDurability::Pending { durable_frontier, .. } => AckDecision::NotYetDurable {
            target: frontier,
            durable_frontier: *durable_frontier,
        },
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
