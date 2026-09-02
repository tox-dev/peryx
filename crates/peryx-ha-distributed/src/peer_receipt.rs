use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
pub use peryx_ha::{PeerReceipt, ReceiptRequest, ReceiptSource};
use peryx_storage::blob::Digest;

use crate::backoff::ReconnectPolicy;
use crate::evidence_gather::{
    Attempt, DEFAULT_GATHER_JITTER, GatherEnd, GatherOutcome, GatherSchedule, Observation, RetiredSources, gather,
    outcome,
};
use crate::filesystem_ack::{FilesystemAck, ReceiptOutcome};
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

/// Polls unconfirmed same-datacenter peers until `ack` reaches quorum or `budget` expires.
///
/// Absent receipts are missing evidence and are retried on the poll cadence. A retryable transport
/// fault is retried too, on widening backoff, until the source spends its attempt limit and is retired
/// for `retry_exhausted`. A terminal fault, including an answer that does not attest `request` from the
/// source's node, retires that source at once. Both retirements are reported in
/// [`GatherOutcome::retired`], where the reason says which happened.
///
/// Reports quorum as [`Deadline::Live`](crate::Deadline::Live). A budget that ran out and a peer set
/// with nothing left to give both report [`Deadline::Expired`](crate::Deadline::Expired), which stays
/// ambiguous because a peer may commit after the client stops waiting. The gather skips sources
/// represented in `ack`, preventing duplicate node receipts from inflating quorum.
pub async fn gather_receipts(
    sources: &[std::sync::Arc<dyn ReceiptSource + Send + Sync>],
    request: ReceiptRequest<'_>,
    ack: &mut FilesystemAck,
    budget: Duration,
    poll: Duration,
) -> GatherOutcome {
    let retired = RetiredSources::default();
    if ack.is_byte_durable() {
        return outcome(GatherEnd::Durable, retired);
    }
    let schedule = GatherSchedule {
        poll,
        policy: ReconnectPolicy::default(),
        jitter: DEFAULT_GATHER_JITTER,
        retired: &retired,
    };
    let end = gather(
        sources
            .iter()
            .filter(|source| !ack.holds(source.node()))
            .map(|source| (source.node(), source.as_ref()))
            .collect(),
        &request,
        budget,
        &schedule,
        |source, request| {
            let retired = &retired;
            Box::pin(async move {
                match source.fetch_receipt(*request).await {
                    Ok(Some(receipt)) => Attempt::Found(receipt),
                    Ok(None) => Attempt::Absent,
                    Err(error) if retired.record(source.node(), &error) => Attempt::Retire,
                    Err(error) => Attempt::Failed(error),
                }
            })
        },
        |receipt| match ack.record(receipt.into()) {
            ReceiptOutcome::Recorded if ack.is_byte_durable() => Observation::Durable,
            ReceiptOutcome::Recorded => Observation::Complete,
            ReceiptOutcome::Ignored => Observation::Pending,
        },
    )
    .await;
    outcome(end, retired)
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

    async fn fetch_receipt(&self, request: ReceiptRequest<'_>) -> Result<Option<PeerReceipt>, TransportError> {
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
            Some((held, size)) if held == request.digest => Ok(Some(PeerReceipt {
                node: self.node.clone(),
                digest: held.clone(),
                size: *size,
            })),
            _ => Ok(None),
        }
    }
}
