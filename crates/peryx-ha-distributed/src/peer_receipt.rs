//! Gathering same-datacenter peers' filesystem placement receipts into one write's byte quorum.
//!
//! A filesystem backend proves datacenter durability only when a quorum of independent nodes each hold
//! the bytes ([`assess_byte_durability`](crate::assess_byte_durability)). The ingress node holds its own
//! receipt the moment its write commits; this is the transport that gathers the rest - it asks each
//! same-datacenter peer whether it durably holds the digest and folds every answer into the write's
//! [`FilesystemAck`]. A peer that does not yet hold the bytes, or that a transport fault hides,
//! does not contribute: it never fails the write, because a missing receipt is absence of proof, not
//! proof of absence.
//!
//! [`gather_receipts`] bounds the whole gather by the client's write deadline. Reaching quorum before
//! the deadline resolves the byte dimension [`Live`](Deadline::Live); spending the deadline without it
//! resolves [`Expired`](Deadline::Expired), which the shared decision turns into a retry-safe
//! [`Unknown`](crate::DcAck::Unknown) rather than a false failure, since a durable copy may land after
//! the client stops waiting. Peers that have not yet replicated the bytes are re-polled across the
//! window, so a receipt that arrives mid-deadline still counts. The gather consults the transport and
//! the clock; the durability arithmetic underneath it stays pure in [`FilesystemAck`].

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
pub use peryx_ha::{PeerReceipt, ReceiptSource};
use peryx_storage::blob::Digest;

use crate::dc_ack::Deadline;
use crate::filesystem_ack::FilesystemAck;
use crate::peer::TransportError;
use crate::receipt_quorum::ReceiptAck;

/// How long the gather waits before re-polling peers that have not yet replicated the bytes. Bounded well
/// under a typical client deadline so a receipt landing mid-window is noticed promptly.
pub const DEFAULT_RECEIPT_POLL: Duration = Duration::from_millis(50);

impl From<PeerReceipt> for ReceiptAck {
    fn from(receipt: PeerReceipt) -> Self {
        Self {
            node: receipt.node,
            digest: receipt.digest,
        }
    }
}

/// Gather same-datacenter peers' receipts into `ack` until it reaches byte quorum or the client `budget`
/// elapses, re-polling on `poll` the peers that do not yet hold the bytes.
///
/// Returns the deadline the shared decision reads: [`Live`](Deadline::Live) once quorum is reached within
/// the budget, [`Expired`](Deadline::Expired) once the budget is spent short of it. A short result is
/// never a definite failure - a durable copy may land after the client stops waiting - so the caller maps
/// [`Expired`](Deadline::Expired) to a retry-safe [`Unknown`](crate::DcAck::Unknown), not an error.
///
/// A quorum already met from the local receipt returns immediately without querying a peer. Each peer is
/// queried at most once per round and skipped once its receipt is held, so a duplicated or repeated
/// answer never inflates the tally and a slow peer never blocks the next round past the budget.
pub async fn gather_receipts(
    sources: &[std::sync::Arc<dyn ReceiptSource + Send + Sync>],
    digest: &Digest,
    ack: &mut FilesystemAck,
    budget: Duration,
    poll: Duration,
) -> Deadline {
    if ack.is_byte_durable() {
        return Deadline::Live;
    }
    let gather = async {
        loop {
            for source in sources {
                if ack.holds(source.node()) {
                    continue;
                }
                if let Ok(Some(receipt)) = source.fetch_receipt(digest).await {
                    ack.record(receipt.into());
                }
            }
            if ack.is_byte_durable() {
                return;
            }
            tokio::time::sleep(poll).await;
        }
    };
    match tokio::time::timeout(budget, gather).await {
        Ok(()) => Deadline::Live,
        Err(_) => Deadline::Expired,
    }
}

/// An in-process [`ReceiptSource`] over a single fixed answer, for driving a gather without a socket.
///
/// It models the three answers a real peer gives: it durably holds the digest ([`holding`](Self::holding)),
/// it does not ([`absent`](Self::absent)), or a transport fault hides it this round ([`inject`](Self::inject)).
/// [`available_after`](Self::available_after) delays the receipt by a number of rounds, so a test drives the
/// re-poll path a still-replicating peer exercises. It answers only for the digest it holds, so a query for
/// another digest is a miss the same way a real store's [`head`](peryx_storage::blob::BlobStorage::head) miss
/// is.
#[derive(Debug)]
pub struct LoopbackReceiptSource {
    node: String,
    held: Option<(Digest, u64)>,
    available_after: usize,
    calls: AtomicUsize,
    fault: Mutex<Option<TransportError>>,
}

impl LoopbackReceiptSource {
    /// A peer that durably holds `digest` at `size` bytes and answers for it immediately.
    #[must_use]
    pub fn holding(node: impl Into<String>, digest: Digest, size: u64) -> Self {
        Self {
            node: node.into(),
            held: Some((digest, size)),
            available_after: 0,
            calls: AtomicUsize::new(0),
            fault: Mutex::new(None),
        }
    }

    /// A peer that holds no receipt for the write, so every query is a miss.
    #[must_use]
    pub fn absent(node: impl Into<String>) -> Self {
        Self {
            node: node.into(),
            held: None,
            available_after: 0,
            calls: AtomicUsize::new(0),
            fault: Mutex::new(None),
        }
    }

    /// Withhold the receipt for the first `rounds` queries, then answer, modeling a peer still replicating
    /// the bytes when the gather begins.
    #[must_use]
    pub const fn available_after(mut self, rounds: usize) -> Self {
        self.available_after = rounds;
        self
    }

    /// Arm the source to fail its next query with `fault`, then clear it. Deterministic stand-in for
    /// transient transport loss.
    pub fn inject(&self, fault: TransportError) {
        *self.fault.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(fault);
    }
}

#[async_trait]
impl ReceiptSource for LoopbackReceiptSource {
    fn node(&self) -> &str {
        &self.node
    }

    async fn fetch_receipt(&self, digest: &Digest) -> Result<Option<PeerReceipt>, TransportError> {
        let fault = self
            .fault
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(fault) = fault {
            return Err(fault);
        }
        let round = self.calls.fetch_add(1, Ordering::Relaxed);
        if round < self.available_after {
            return Ok(None);
        }
        match &self.held {
            Some((held, size)) if held == digest => Ok(Some(PeerReceipt {
                node: self.node.clone(),
                digest: digest.clone(),
                size: *size,
            })),
            _ => Ok(None),
        }
    }
}
