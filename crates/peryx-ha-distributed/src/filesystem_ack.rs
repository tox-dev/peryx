//! Accumulating a filesystem write's datacenter acknowledgement from independent per-node receipts.
//!
//! A filesystem backend proves datacenter durability only when a configured quorum of independent nodes
//! each return a verified placement receipt. This is the mechanism's state: it holds the receipts one
//! write has gathered so far, dropping a receipt for another digest or a second receipt from a node it
//! already holds, and turns the current evidence into the client's acknowledgement through
//! [`decide_dc_ack`](crate::decide_dc_ack). Holding the receipts is what preserves a write's completed
//! copies for a safe retry: a retry resumes from the quorum it had reached rather than re-copying, and a
//! lost or duplicated response never inflates the tally.
//!
//! Dispatching to peers and awaiting their receipts under a deadline is the caller's; this is the
//! deterministic core it feeds and reads.

use std::collections::BTreeSet;

use peryx_storage::blob::Digest;

use crate::ack::AckDecision;
use crate::byte_ack::{ByteAckDecision, decide_byte_ack};
use crate::dc_ack::{ByteEvidence, DcAck, Deadline, decide_dc_ack};
use crate::readiness::DurabilityPolicy;
use crate::receipt_quorum::ReceiptAck;

/// Whether recording a receipt advanced a write's evidence or was dropped as a duplicate or misroute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptOutcome {
    /// A receipt from a new independent node for this digest; the quorum advanced.
    Recorded,
    /// A receipt for another digest, or a second one from a node this write already holds; nothing
    /// changed.
    Ignored,
}

/// One filesystem write's gathered receipts, held against its digest, member set, and durability
/// policy to decide its datacenter acknowledgement.
#[derive(Debug, Clone)]
pub struct FilesystemAck {
    digest: Digest,
    members: BTreeSet<String>,
    policy: DurabilityPolicy,
    receipts: Vec<ReceiptAck>,
}

impl FilesystemAck {
    /// A write awaiting quorum for `digest` across the configured `members` under `policy`.
    #[must_use]
    pub const fn new(digest: Digest, members: BTreeSet<String>, policy: DurabilityPolicy) -> Self {
        Self {
            digest,
            members,
            policy,
            receipts: Vec::new(),
        }
    }

    /// Record one node's receipt. A receipt for another digest, one from a node outside the configured
    /// members, or a second receipt from a node this write already holds is
    /// [`Ignored`](ReceiptOutcome::Ignored), so a duplicated, misrouted, or stray response never inflates
    /// the quorum and the held set stays bounded by the members.
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

    /// The number of independent nodes whose receipts this write holds.
    #[must_use]
    pub const fn independent_receipts(&self) -> usize {
        self.receipts.len()
    }

    /// Whether a receipt from `node` is already held, so the gather skips re-querying a peer that has
    /// already proven durability.
    #[must_use]
    pub fn holds(&self, node: &str) -> bool {
        self.receipts.iter().any(|held| held.node == node)
    }

    /// The current filesystem byte decision from the receipts held so far: acknowledged, or pending with
    /// how many more independent receipts remain. Exposed so a caller can record the quorum progress
    /// behind the byte dimension without re-deriving it.
    #[must_use]
    pub fn byte_decision(&self) -> ByteAckDecision {
        decide_byte_ack(&self.digest, &self.receipts, &self.members, self.policy)
    }

    /// Whether the held receipts already meet the datacenter byte quorum, so the gather can stop before
    /// the deadline once no further peer is needed.
    #[must_use]
    pub fn is_byte_durable(&self) -> bool {
        self.byte_decision().is_acknowledged()
    }

    /// The write's datacenter acknowledgement given its `metadata` decision and `deadline`.
    ///
    /// Folds the held receipts into the filesystem byte dimension and combines it with the metadata
    /// dimension through [`decide_dc_ack`](crate::decide_dc_ack): durable once both prove, pending while
    /// the deadline is live, and unknown rather than failed once the deadline expires unproven.
    #[must_use]
    pub fn decide(&self, metadata: AckDecision, deadline: Deadline) -> DcAck {
        let byte_ack = decide_byte_ack(&self.digest, &self.receipts, &self.members, self.policy);
        decide_dc_ack(metadata, &ByteEvidence::Filesystem(byte_ack), deadline)
    }
}
