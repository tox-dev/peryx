//! Driving a fenced authority transfer from a plan to a sealed, persisted audit.
//!
//! [`TransferPlan`](peryx_ha_distributed::TransferPlan) is the pure decision core: it waits at
//! `AwaitingCatchUp` until the target's applied frontier reaches the barrier, then stands `Ready` for a
//! caller to commit. This module supplies the two things it reaches for from above — the target
//! datacenter's applied frontier and the committed move — without pulling any I/O into the plan itself.
//!
//! [`observe_target`] probes the target's frontier once and folds it into the plan, so a loop above can
//! poll it toward `Ready`. [`commit_transfer`] takes a ready plan, commits the move through the ownership
//! consensus group, reads the epoch that commit minted back from committed state, seals the audit, and
//! persists it. Splitting the two keeps the barrier wait and the single committed move distinct, so a
//! cancel that races the commit resolves against the plan rather than a half-applied move.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use peryx_driver::state::{ControlCommand, ControlError, ControlPlane, OwnershipAuthority};
use peryx_ha_distributed::{
    AuthorityEpoch, BatchRequest, DEFAULT_TRANSFER_LIMITS, HttpPeerTransport, PeerTransport, TransferAudit,
    TransferError, TransferPhase, TransferPlan, TransferRequest,
};
use peryx_storage::meta::{MetaError, MetaStore, TransferAudit as StoredTransferAudit};

/// How long a frontier probe waits for the target's change-feed to answer before it reads as unreachable.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Reads a datacenter's applied metadata frontier, the highest serial it has durably applied, so the
/// barrier gate can tell when the target has replicated the source's writes.
///
/// The production source probes the target's change-feed; a test double returns a scripted frontier.
#[async_trait::async_trait]
pub trait FrontierSource: Send + Sync {
    /// The highest metadata serial `datacenter` has durably applied, or `None` when it cannot be reached
    /// or has applied nothing yet.
    ///
    /// # Errors
    /// Returns an error when the target cannot be reached or answers unusably.
    async fn applied_frontier(&self, datacenter: &str) -> anyhow::Result<Option<u64>>;
}

/// Reads the committed authority epoch back from ownership state after a move commits.
///
/// A narrow read seam over the ownership consensus group so the commit path derives the epoch the audit
/// records from committed state rather than the receipt.
#[async_trait::async_trait]
pub trait EpochOracle: Send + Sync {
    /// The committed epoch for `authority`, the value the just-committed move minted.
    async fn committed_epoch(&self, authority: &str) -> u64;
}

/// Why driving a transfer did not seal an audit.
#[derive(Debug, thiserror::Error)]
pub enum TransferDriveError {
    /// Reading the target's applied frontier failed.
    #[error("read the target frontier: {0}")]
    Frontier(#[source] anyhow::Error),
    /// The consensus group refused the committed move.
    #[error("commit the transfer: {0}")]
    Commit(#[source] ControlError),
    /// The plan refused the commit: it was cancelled, or the barrier was not met.
    #[error("seal the transfer: {0}")]
    Plan(#[source] TransferError),
    /// Persisting the sealed audit failed.
    #[error("persist the transfer audit: {0}")]
    Persist(#[source] MetaError),
}

/// Probe the target's applied frontier and record it on `plan`, returning where the plan now stands.
///
/// A target that cannot be reached reads as frontier zero, which never advances a plan past its barrier,
/// so an unreachable target leaves the move waiting rather than committing it early.
///
/// # Errors
/// Returns [`TransferDriveError::Frontier`] when the frontier read fails.
pub async fn observe_target(
    plan: &mut TransferPlan,
    frontier: &dyn FrontierSource,
) -> Result<TransferPhase, TransferDriveError> {
    let applied = frontier
        .applied_frontier(&plan.request().target.0)
        .await
        .map_err(TransferDriveError::Frontier)?
        .unwrap_or(0);
    Ok(plan.observe_frontier(applied))
}

/// Commit a ready `plan` through the ownership consensus group, derive the epoch the commit minted from
/// committed state, seal the audit, and persist it.
///
/// The commit rides [`ControlPlane::execute`], so an `Idempotency-Key` deduplicates a retry across a
/// leader loss to one committed move, and the committed log index it returns is the index the audit
/// records. The epoch comes from a read of committed ownership state rather than the receipt, matching the
/// rest of the ownership plane, which surfaces effects through committed reads.
///
/// # Errors
/// Returns [`TransferDriveError`] when the commit is refused, the plan refuses the commit, or persisting
/// the audit fails.
pub async fn commit_transfer(
    plan: &mut TransferPlan,
    control: &ControlPlane,
    ownership: &dyn EpochOracle,
    meta: &MetaStore,
    key: Option<&str>,
) -> Result<TransferAudit, TransferDriveError> {
    // Resolve the plan's standing before reaching for consensus: only a ready plan submits the move, so a
    // cancel that won the race, or a barrier not yet met, never commits a move the plan already refused.
    let (actor, command) = match plan.phase() {
        TransferPhase::Ready => {
            let request = plan.request();
            (
                request.actor.clone(),
                ControlCommand::TransferAuthority {
                    authority: request.authority.0.clone(),
                    new_home: request.target.0.clone(),
                },
            )
        }
        // Already sealed: return the audit the first commit booked without submitting a second move. The
        // epoch and index are ignored for a committed plan, which reads back its existing record.
        TransferPhase::Committed => return plan.commit(AuthorityEpoch(0), 0).map_err(TransferDriveError::Plan),
        TransferPhase::Cancelled => return Err(TransferDriveError::Plan(TransferError::Cancelled)),
        TransferPhase::AwaitingCatchUp => return Err(TransferDriveError::Plan(TransferError::BarrierNotMet)),
    };
    let receipt = control
        .execute(&actor, key, command)
        .await
        .map_err(TransferDriveError::Commit)?;
    let epoch = ownership.committed_epoch(&plan.request().authority.0).await;
    let audit = plan
        .commit(AuthorityEpoch(epoch), receipt.index)
        .map_err(TransferDriveError::Plan)?;
    meta.record_transfer_audit(&stored(&audit))
        .map_err(TransferDriveError::Persist)?;
    Ok(audit)
}

/// The ownership consensus group reads back a committed epoch, so a driver can derive it from committed
/// state after a move without threading it through the receipt.
#[async_trait::async_trait]
impl EpochOracle for Arc<dyn OwnershipAuthority> {
    async fn committed_epoch(&self, authority: &str) -> u64 {
        OwnershipAuthority::committed_epoch(self.as_ref(), authority).await
    }
}

/// How often the coordinator re-probes the target's frontier while a plan waits at its barrier.
const DEFAULT_POLL: Duration = Duration::from_secs(2);

/// How many frontier probes a single transfer may spend waiting for its target to catch up before the
/// coordinator gives up. At the default poll this bounds one transfer to roughly five minutes of waiting.
const DEFAULT_BUDGET: u32 = 150;

/// Why the coordinator did not seal a transfer.
#[derive(Debug, thiserror::Error)]
pub enum TransferRunError {
    /// A transfer for this authority is already running, so a second request is refused rather than
    /// racing two moves toward the same home.
    #[error("a transfer for {0} is already running")]
    Busy(String),
    /// The target did not reach the barrier within the observation budget, so the move never became
    /// eligible to commit.
    #[error("the target did not reach the transfer barrier within the observation budget")]
    BarrierNotReached,
    /// Driving the ready plan to a sealed audit failed.
    #[error(transparent)]
    Drive(#[from] TransferDriveError),
}

/// Why the coordinator did not cancel a transfer.
#[derive(Debug, thiserror::Error)]
pub enum TransferCancelError {
    /// No transfer is registered for this authority, so there is nothing to cancel.
    #[error("no transfer is registered for {0}")]
    Unknown(String),
    /// The transfer already committed, so a cancel that lost the race to the commit is refused and the
    /// move stands.
    #[error("the transfer for {0} already committed and cannot be cancelled")]
    AlreadyCommitted(String),
}

/// One authority's transfer as the coordinator tracks it: the shared plan a run drives and a cancel
/// locks, and whether a run currently owns it.
struct Registered {
    plan: Arc<tokio::sync::Mutex<TransferPlan>>,
    active: Arc<AtomicBool>,
}

/// Owns the transfers a node is running: one at a time per authority, each driven from its request to a
/// sealed audit, and each cancellable while it waits.
///
/// A run registers its plan under the authority, drives it toward `Ready`, commits it, then clears the
/// active flag while keeping the resolved plan. Retaining the plan lets a cancel that arrives after the
/// commit resolve against the sealed record — [`AlreadyCommitted`](TransferCancelError::AlreadyCommitted)
/// rather than a lost lookup — so the two operations never disagree on whether the move happened.
pub struct TransferCoordinator {
    frontier: Arc<dyn FrontierSource>,
    poll: Duration,
    budget: u32,
    registry: Mutex<HashMap<String, Registered>>,
}

impl TransferCoordinator {
    /// Build a coordinator that probes `frontier` on the default poll and observation budget.
    #[must_use]
    pub fn new(frontier: Arc<dyn FrontierSource>) -> Self {
        Self::with_schedule(frontier, DEFAULT_POLL, DEFAULT_BUDGET)
    }

    /// Build a coordinator with an explicit `poll` interval and observation `budget`, so a test can drive
    /// the wait loop without wall-clock delay.
    #[must_use]
    pub fn with_schedule(frontier: Arc<dyn FrontierSource>, poll: Duration, budget: u32) -> Self {
        Self {
            frontier,
            poll,
            budget,
            registry: Mutex::new(HashMap::new()),
        }
    }

    /// Register `request` and drive it to a sealed audit: wait for the target to reach the barrier, then
    /// commit the move through `control`, derive its epoch from `ownership`, and persist the audit to
    /// `meta`.
    ///
    /// One transfer runs per authority at a time; a second request while one is active is
    /// [`Busy`](TransferRunError::Busy). The active flag clears when the drive resolves, but the plan
    /// stays registered so a later cancel resolves against it.
    ///
    /// # Errors
    /// Returns [`Busy`](TransferRunError::Busy) when a transfer for the authority is already running,
    /// [`BarrierNotReached`](TransferRunError::BarrierNotReached) when the budget runs out before the
    /// target catches up, or [`Drive`](TransferRunError::Drive) when committing or persisting the move
    /// fails.
    pub async fn run(
        &self,
        request: TransferRequest,
        control: &ControlPlane,
        ownership: &dyn EpochOracle,
        meta: &MetaStore,
        key: Option<&str>,
    ) -> Result<TransferAudit, TransferRunError> {
        let authority = request.authority.0.clone();
        let (plan, active) = {
            // Recover a poisoned guard rather than panic: the registry is a lookup table no panic can
            // leave inconsistent.
            let mut registry = self.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if registry
                .get(&authority)
                .is_some_and(|registered| registered.active.load(Ordering::Acquire))
            {
                return Err(TransferRunError::Busy(authority));
            }
            let plan = Arc::new(tokio::sync::Mutex::new(TransferPlan::plan(request)));
            let active = Arc::new(AtomicBool::new(true));
            registry.insert(
                authority,
                Registered {
                    plan: plan.clone(),
                    active: active.clone(),
                },
            );
            drop(registry);
            (plan, active)
        };
        let outcome = self.drive(&plan, control, ownership, meta, key).await;
        // Release the authority for a later transfer while keeping its plan registered, so a cancel that
        // arrives after this run resolves against the sealed record rather than a missing entry.
        active.store(false, Ordering::Release);
        outcome
    }

    /// Poll the target's frontier until the plan is ready, then commit it. A plan resolved out from under
    /// the wait — a cancel that won the race — falls straight through to the commit, which refuses it.
    async fn drive(
        &self,
        plan: &tokio::sync::Mutex<TransferPlan>,
        control: &ControlPlane,
        ownership: &dyn EpochOracle,
        meta: &MetaStore,
        key: Option<&str>,
    ) -> Result<TransferAudit, TransferRunError> {
        for _ in 0..self.budget {
            let mut plan = plan.lock().await;
            if matches!(
                observe_target(&mut plan, self.frontier.as_ref()).await?,
                TransferPhase::AwaitingCatchUp
            ) {
                drop(plan);
                tokio::time::sleep(self.poll).await;
                continue;
            }
            return commit_transfer(&mut plan, control, ownership, meta, key)
                .await
                .map_err(TransferRunError::Drive);
        }
        Err(TransferRunError::BarrierNotReached)
    }

    /// Cancel the transfer registered for `authority`, abandoning a plan that has not committed.
    ///
    /// # Errors
    /// Returns [`Unknown`](TransferCancelError::Unknown) when no transfer is registered, or
    /// [`AlreadyCommitted`](TransferCancelError::AlreadyCommitted) when the move already committed.
    pub async fn cancel(&self, authority: &str) -> Result<(), TransferCancelError> {
        let plan = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(authority)
            .map(|registered| registered.plan.clone());
        let Some(plan) = plan else {
            return Err(TransferCancelError::Unknown(authority.to_owned()));
        };
        // A plan's own guard rejects a cancel only when it has committed, which is the sole error
        // `cancel` yields, so any refusal maps to that outcome.
        plan.lock()
            .await
            .cancel()
            .map_err(|_| TransferCancelError::AlreadyCommitted(authority.to_owned()))
    }
}

/// Probes a datacenter's change-feed for its applied frontier, resolving the datacenter to a base address
/// from the availability roster.
///
/// The feed's `current_serial` is the highest metadata serial that datacenter has durably applied. A
/// datacenter absent from the roster, or one whose feed cannot be reached or has synced nothing, reads as
/// no frontier so the barrier gate keeps the move waiting rather than committing to a target that has not
/// caught up.
pub struct RosterFrontierSource {
    peers: Vec<(String, String)>,
    token: String,
}

impl RosterFrontierSource {
    /// Build a source over `peers`, each a `(datacenter, base URL)` pair, authenticating every probe with
    /// `token`.
    #[must_use]
    pub fn new(peers: Vec<(String, String)>, token: impl Into<String>) -> Self {
        Self {
            peers,
            token: token.into(),
        }
    }
}

#[async_trait::async_trait]
impl FrontierSource for RosterFrontierSource {
    async fn applied_frontier(&self, datacenter: &str) -> anyhow::Result<Option<u64>> {
        let Some((_, address)) = self.peers.iter().find(|(name, _)| name == datacenter) else {
            return Ok(None);
        };
        let transport = HttpPeerTransport::new(address, self.token.clone(), DEFAULT_TRANSFER_LIMITS, PROBE_TIMEOUT)?;
        let request = BatchRequest {
            after: 0,
            max_operations: NonZeroUsize::new(1).expect("1 is non-zero"),
        };
        // An unreachable target, or one that has synced no source yet, reads as no frontier so the move
        // keeps waiting rather than committing to a target that has not caught up.
        transport
            .fetch_batch(request)
            .await
            .map_or_else(|_| Ok(None), |frame| Ok(Some(frame.frontier().1)))
    }
}

/// Flatten a sealed audit into the primitive record the store persists.
fn stored(audit: &TransferAudit) -> StoredTransferAudit {
    StoredTransferAudit {
        authority: audit.authority.0.clone(),
        source: audit.source.0.clone(),
        target: audit.target.0.clone(),
        actor: audit.actor.clone(),
        reason: audit.reason.clone(),
        barrier: audit.barrier,
        epoch: audit.epoch.0,
        commit_index: audit.commit_index,
    }
}

#[cfg(test)]
mod tests;
