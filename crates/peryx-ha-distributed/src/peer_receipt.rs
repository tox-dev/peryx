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

pub const DEFAULT_RECEIPT_POLL: Duration = Duration::from_millis(50);

impl From<PeerReceipt> for ReceiptAck {
    fn from(receipt: PeerReceipt) -> Self {
        Self {
            node: receipt.node,
            digest: receipt.digest,
        }
    }
}

/// Polls unconfirmed same-datacenter peers until `ack` reaches quorum or `budget` expires. The gather
/// treats transport faults and absent receipts as missing evidence and retries them.
///
/// Returns [`Deadline::Live`] for quorum and [`Deadline::Expired`] for timeout. Expiration remains
/// ambiguous because a peer may commit after the client stops waiting. The gather skips sources
/// represented in `ack`, preventing duplicate node receipts from inflating quorum.
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

#[derive(Debug)]
pub struct LoopbackReceiptSource {
    node: String,
    held: Option<(Digest, u64)>,
    available_after: usize,
    calls: AtomicUsize,
    fault: Mutex<Option<TransportError>>,
}

impl LoopbackReceiptSource {
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

    /// Suppresses the first `rounds` queries.
    #[must_use]
    pub const fn available_after(mut self, rounds: usize) -> Self {
        self.available_after = rounds;
        self
    }

    /// Makes the next query return `fault`.
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
