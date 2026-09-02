//! Polls remote metadata frontiers until the configured write-ack policy's share of datacenters proves
//! the operation durable. Missing, failed, or wrong-epoch reports leave durability unproven. Deadline
//! expiry remains retry-safe because a remote may commit after polling stops.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
pub use peryx_ha::RemoteFrontierSource;
use peryx_ha::{RemoteAck, TransportError};

use crate::evidence_gather::{Attempt, GatherEnd, GatherOutcome, Observation, RetiredSources, gather, outcome};
use crate::remote_durability::{DurabilityPolicy, MetadataOperation, assess_remote_metadata_durability};

pub const DEFAULT_FRONTIER_POLL: Duration = Duration::from_millis(50);

/// Proves remote metadata durability, or reports why it could not.
///
/// Reports [`Deadline::Live`](crate::Deadline::Live) once `acks` prove durability under `policy`,
/// including evidence present on entry. `sources` is the configured remote datacenter set the policy
/// resolves against.
///
/// A missing report and a retryable transport fault are both absent evidence and are polled again. A
/// terminal fault cannot be revised by another poll, so it retires that datacenter for the rest of the
/// write and is reported in [`GatherOutcome::retired`] rather than dropped.
///
/// Otherwise reports [`Deadline::Expired`](crate::Deadline::Expired), either when `budget` runs out
/// or once no remaining datacenter can report. Each datacenter's latest report replaces its prior
/// report, so duplicates cannot inflate durability.
pub async fn gather_remote_acks(
    sources: &[std::sync::Arc<dyn RemoteFrontierSource + Send + Sync>],
    authority: &str,
    operation: &MetadataOperation,
    acks: &mut Vec<RemoteAck>,
    policy: DurabilityPolicy,
    budget: Duration,
    poll: Duration,
) -> GatherOutcome {
    let configured = sources.len();
    let retired = RetiredSources::default();
    if assess_remote_metadata_durability(operation, acks, configured, policy).is_durable() {
        sort_acks(acks);
        return outcome(GatherEnd::Durable, retired);
    }
    let end = gather(
        sources.iter().map(AsRef::as_ref).collect(),
        authority,
        budget,
        poll,
        |source, authority| {
            let retired = &retired;
            Box::pin(async move {
                match source.fetch_frontier(authority).await {
                    Ok(Some(ack)) => Attempt::Found(ack),
                    Err(error) if retired.record(source.datacenter(), &error) => Attempt::Retire,
                    Ok(None) | Err(_) => Attempt::Retry,
                }
            })
        },
        |ack| {
            acks.retain(|held| held.datacenter != ack.datacenter);
            acks.push(ack);
            if assess_remote_metadata_durability(operation, acks, configured, policy).is_durable() {
                Observation::Durable
            } else {
                Observation::Pending
            }
        },
    )
    .await;
    sort_acks(acks);
    outcome(end, retired)
}

fn sort_acks(acks: &mut [RemoteAck]) {
    acks.sort_by(|left, right| left.datacenter.cmp(&right.datacenter));
}

#[derive(Debug)]
pub struct LoopbackRemoteFrontierSource {
    datacenter: String,
    reports: Option<(u64, u64)>,
    available_after: usize,
    calls: AtomicUsize,
    fault: Mutex<Option<TransportError>>,
}

impl LoopbackRemoteFrontierSource {
    #[must_use]
    pub fn reporting(datacenter: impl Into<String>, epoch: u64, applied_frontier: u64) -> Self {
        Self {
            datacenter: datacenter.into(),
            reports: Some((epoch, applied_frontier)),
            available_after: 0,
            calls: AtomicUsize::new(0),
            fault: Mutex::new(None),
        }
    }

    #[must_use]
    pub fn silent(datacenter: impl Into<String>) -> Self {
        Self {
            datacenter: datacenter.into(),
            reports: None,
            available_after: 0,
            calls: AtomicUsize::new(0),
            fault: Mutex::new(None),
        }
    }

    #[must_use]
    pub const fn available_after(mut self, rounds: usize) -> Self {
        self.available_after = rounds;
        self
    }

    pub fn inject(&self, fault: TransportError) {
        *self.fault.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(fault);
    }
}

#[async_trait]
impl RemoteFrontierSource for LoopbackRemoteFrontierSource {
    fn datacenter(&self) -> &str {
        &self.datacenter
    }

    async fn fetch_frontier(&self, _authority: &str) -> Result<Option<RemoteAck>, TransportError> {
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
        Ok(self.reports.map(|(epoch, applied_frontier)| RemoteAck {
            datacenter: self.datacenter.clone(),
            epoch,
            applied_frontier,
        }))
    }
}
