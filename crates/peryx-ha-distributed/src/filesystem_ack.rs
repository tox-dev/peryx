//! Tracks independent filesystem receipts for one write. Receipts from another digest, unknown nodes,
//! and duplicate nodes do not count; retained receipts preserve quorum progress across retries.

use std::collections::BTreeSet;

use peryx_storage::blob::Digest;

use crate::byte_ack::decide_byte_ack;
use crate::dc_ack::ByteEvidence;
use crate::readiness::DurabilityPolicy;
use crate::receipt_quorum::ReceiptAck;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptOutcome {
    Recorded,
    Ignored,
}

#[derive(Debug, Clone)]
pub struct FilesystemAck {
    digest: Digest,
    members: BTreeSet<String>,
    policy: DurabilityPolicy,
    receipts: Vec<ReceiptAck>,
}

impl FilesystemAck {
    #[must_use]
    pub const fn new(digest: Digest, members: BTreeSet<String>, policy: DurabilityPolicy) -> Self {
        Self {
            digest,
            members,
            policy,
            receipts: Vec::new(),
        }
    }

    /// Ignores receipts for another digest, outside the configured membership, or from a node already
    /// counted.
    pub fn record(&mut self, receipt: ReceiptAck) -> ReceiptOutcome {
        if receipt.digest != self.digest
            || !self.members.contains(&receipt.node)
            || self.receipts.iter().any(|held| held.node == receipt.node)
        {
            return ReceiptOutcome::Ignored;
        }
        self.receipts.push(receipt);
        ReceiptOutcome::Recorded
    }

    #[must_use]
    pub const fn independent_receipts(&self) -> usize {
        self.receipts.len()
    }

    #[must_use]
    pub fn holds(&self, node: &str) -> bool {
        self.receipts.iter().any(|held| held.node == node)
    }

    /// The receipts gathered so far, as the evidence a datacenter acknowledgement weighs.
    #[must_use]
    pub fn evidence(&self) -> ByteEvidence {
        ByteEvidence::Filesystem(decide_byte_ack(
            &self.digest,
            &self.receipts,
            &self.members,
            self.policy,
        ))
    }

    #[must_use]
    pub fn is_byte_durable(&self) -> bool {
        self.evidence().is_durable()
    }
}
