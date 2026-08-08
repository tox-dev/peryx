//! Gathering eligible remote datacenters' metadata apply-frontiers into one HA write's durability.
//!
//! HA success requires one eligible remote datacenter to commit the exact metadata operation
//! ([`assess_remote_metadata_durability`](crate::assess_remote_metadata_durability)). The ingress writer
//! commits the operation to its own journal at once; this is the transport that gathers the rest - it
//! asks each eligible remote how far it has durably applied under which authority epoch and folds every
//! answer against the operation. A remote that has not yet applied through the frontier, that a fault
//! hides, or that reports a stale or advanced epoch does not contribute: it never fails the write,
//! because a missing acknowledgement is absence of proof, not proof of absence.
//!
//! [`gather_remote_acks`] bounds the whole gather by the client's write deadline. Reaching remote
//! durability before the deadline resolves it [`Live`](Deadline::Live); spending the deadline without it
//! resolves [`Expired`](Deadline::Expired), which the write turns into a retry-safe outcome rather than a
//! false failure, since a remote may commit after the client stops waiting and a retry then resolves to
//! success. Each remote is re-polled across the window, so an acknowledgement that lands mid-deadline
//! still counts, and a remote's latest frontier replaces its earlier one so a slow apply never sticks.
//! The gather consults the transport and the clock; the durability fold underneath it stays pure.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
pub use peryx_ha::RemoteFrontierSource;
use peryx_ha::{RemoteAck, TransportError};

use crate::dc_ack::Deadline;
use crate::remote_durability::{MetadataOperation, assess_remote_metadata_durability};

/// How long the gather waits before re-polling remotes that have not yet applied the operation. Bounded
/// well under a typical client deadline so an acknowledgement landing mid-window is noticed promptly.
pub const DEFAULT_FRONTIER_POLL: Duration = Duration::from_millis(50);

/// Gather eligible remote datacenters' acknowledgements into `acks` until the operation is remote-durable
/// or the budget elapses.
///
/// One eligible remote proving `operation` durable ends the gather; otherwise it re-polls on `poll` the
/// remotes that have not yet applied it, until the client `budget` is spent.
///
/// Returns the deadline the write reads: [`Live`](Deadline::Live) once an eligible remote holds the
/// operation within the budget, [`Expired`](Deadline::Expired) once the budget is spent short of it. A
/// short result is never a definite failure - a remote may commit after the client stops waiting - so the
/// caller maps [`Expired`](Deadline::Expired) to a retry-safe outcome, and a later retry resolves to
/// success once the remote has applied.
///
/// A remote's latest acknowledgement replaces its earlier one in `acks`, so a duplicated or reordered
/// answer never inflates the holder set and an advancing frontier is always read at its newest value. A
/// durability already met from acknowledgements held in `acks` returns immediately without querying a
/// remote.
pub async fn gather_remote_acks(
    sources: &[std::sync::Arc<dyn RemoteFrontierSource + Send + Sync>],
    authority: &str,
    operation: &MetadataOperation,
    acks: &mut Vec<RemoteAck>,
    budget: Duration,
    poll: Duration,
) -> Deadline {
    if assess_remote_metadata_durability(operation, acks).is_durable() {
        return Deadline::Live;
    }
    let gather = async {
        loop {
            for source in sources {
                if let Ok(Some(ack)) = source.fetch_frontier(authority).await {
                    acks.retain(|held| held.datacenter != ack.datacenter);
                    acks.push(ack);
                }
            }
            if assess_remote_metadata_durability(operation, acks).is_durable() {
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

/// An in-process [`RemoteFrontierSource`] over a single fixed answer, for driving a gather without a
/// socket.
///
/// It models the answers a real remote gives: an acknowledgement at an epoch and frontier
/// ([`reporting`](Self::reporting)), no report yet ([`silent`](Self::silent)), or a transport fault this
/// round ([`inject`](Self::inject)). [`available_after`](Self::available_after) delays the report by a
/// number of rounds, so a test drives the re-poll path a remote still applying the operation exercises.
#[derive(Debug)]
pub struct LoopbackRemoteFrontierSource {
    datacenter: String,
    reports: Option<(u64, u64)>,
    available_after: usize,
    calls: AtomicUsize,
    fault: Mutex<Option<TransportError>>,
}

impl LoopbackRemoteFrontierSource {
    /// A remote that reports it has applied through `applied_frontier` under `epoch`, answering at once.
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

    /// A remote that reports no frontier, so every query is a miss.
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

    /// Withhold the report for the first `rounds` queries, then answer, modeling a remote still applying
    /// the operation when the gather begins.
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
