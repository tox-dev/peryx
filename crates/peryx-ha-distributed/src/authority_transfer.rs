//! Consensus receives a move after the target reaches the source barrier. Cancellation and commit
//! resolve against the same plan, preventing a cancelled move from reaching consensus.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::{
    AuthorityEpoch, BatchRequest, DEFAULT_TRANSFER_LIMITS, HttpPeerTransport, PeerTransport, TransferAudit,
    TransferError, TransferPhase, TransferPlan, TransferRequest,
};
use peryx_ha::TransferAudit as StoredTransferAudit;
use peryx_ha::{ControlCommand, ControlError, OwnershipAuthority};
use peryx_storage::meta::{MetaError, MetaStore};

const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Supplies the highest metadata serial persisted by a datacenter.
#[async_trait::async_trait]
pub trait FrontierSource: Send + Sync {
    /// Returns `None` when the source has no frontier.
    ///
    /// # Errors
    /// Returns an error when the source cannot produce a usable answer.
    async fn applied_frontier(&self, datacenter: &str) -> anyhow::Result<Option<u64>>;
}

/// Reads audit epochs from committed ownership state rather than command receipts.
#[async_trait::async_trait]
pub trait EpochOracle: Send + Sync {
    async fn committed_epoch(&self, authority: &str) -> u64;
}

#[derive(Debug, thiserror::Error)]
pub enum TransferDriveError {
    #[error("read the target frontier: {0}")]
    Frontier(#[source] anyhow::Error),
    #[error("commit the transfer: {0}")]
    Commit(#[source] ControlError),
    #[error("seal the transfer: {0}")]
    Plan(#[source] TransferError),
    #[error("persist the transfer audit: {0}")]
    Persist(#[source] MetaError),
}

/// Treats a missing frontier as zero so it cannot satisfy the transfer barrier.
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

/// Commits a ready plan, then seals and persists its audit.
///
/// An idempotency key deduplicates retries across leader loss. The audit records the committed log index
/// and reads its epoch from committed ownership state.
///
/// # Errors
/// Returns [`TransferDriveError`] when the commit is refused, the plan refuses the commit, or persisting
/// the audit fails.
pub async fn commit_transfer(
    plan: &mut TransferPlan,
    control: &dyn peryx_ha::ControlExecutor,
    ownership: &dyn EpochOracle,
    meta: &MetaStore,
    key: Option<&str>,
) -> Result<TransferAudit, TransferDriveError> {
    // Prevent cancelled or unready plans from reaching consensus.
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
        // A sealed plan returns its original audit without submitting another move; commit ignores these
        // placeholder values in this state.
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

#[async_trait::async_trait]
impl EpochOracle for Arc<dyn OwnershipAuthority> {
    async fn committed_epoch(&self, authority: &str) -> u64 {
        OwnershipAuthority::committed_epoch(self.as_ref(), authority).await
    }
}

const DEFAULT_POLL: Duration = Duration::from_secs(2);

const DEFAULT_BUDGET: u32 = 150;

#[derive(Debug, thiserror::Error)]
pub enum TransferRunError {
    #[error("a transfer for {0} is already running")]
    Busy(String),
    #[error("the target did not reach the transfer barrier within the observation budget")]
    BarrierNotReached,
    #[error(transparent)]
    Drive(#[from] TransferDriveError),
}

#[derive(Debug, thiserror::Error)]
pub enum TransferCancelError {
    #[error("no transfer is registered for {0}")]
    Unknown(String),
    #[error("the transfer for {0} already committed and cannot be cancelled")]
    AlreadyCommitted(String),
}

struct Registered {
    plan: Arc<tokio::sync::Mutex<TransferPlan>>,
    active: Arc<AtomicBool>,
}

/// Runs one transfer per authority and retains resolved plans so late cancellation observes a commit.
pub struct TransferCoordinator {
    frontier: Arc<dyn FrontierSource>,
    poll: Duration,
    budget: u32,
    registry: Mutex<HashMap<String, Registered>>,
}

impl TransferCoordinator {
    #[must_use]
    pub fn new(frontier: Arc<dyn FrontierSource>) -> Self {
        Self::with_schedule(frontier, DEFAULT_POLL, DEFAULT_BUDGET)
    }

    #[must_use]
    pub fn with_schedule(frontier: Arc<dyn FrontierSource>, poll: Duration, budget: u32) -> Self {
        Self {
            frontier,
            poll,
            budget,
            registry: Mutex::new(HashMap::new()),
        }
    }

    /// Runs one transfer per authority. The resolved plan remains registered for later cancellation.
    ///
    /// # Errors
    /// Returns [`Busy`](TransferRunError::Busy) when a transfer for the authority is already running,
    /// [`BarrierNotReached`](TransferRunError::BarrierNotReached) when the budget runs out before the
    /// target catches up, or [`Drive`](TransferRunError::Drive) when committing or persisting the move
    /// fails.
    pub async fn run(
        &self,
        request: TransferRequest,
        control: &dyn peryx_ha::ControlExecutor,
        ownership: &dyn EpochOracle,
        meta: &MetaStore,
        key: Option<&str>,
    ) -> Result<TransferAudit, TransferRunError> {
        let authority = request.authority.0.clone();
        let (plan, active) = {
            // A panic cannot corrupt this lookup table, so recover its poisoned guard.
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
        // Keep the resolved plan so a late cancel observes its outcome instead of a missing entry.
        active.store(false, Ordering::Release);
        outcome
    }

    /// A concurrent cancellation reaches `commit_transfer`, which rejects the cancelled plan.
    async fn drive(
        &self,
        plan: &tokio::sync::Mutex<TransferPlan>,
        control: &dyn peryx_ha::ControlExecutor,
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
        // `TransferPlan::cancel` rejects committed plans.
        plan.lock()
            .await
            .cancel()
            .map_err(|_| TransferCancelError::AlreadyCommitted(authority.to_owned()))
    }
}

/// Returns no frontier for missing, unreachable, or unsynced datacenters, preventing barrier admission.
pub struct RosterFrontierSource {
    peers: Vec<(String, String)>,
    token: String,
}

impl RosterFrontierSource {
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
        // Treat probe failure as no frontier so it cannot satisfy the barrier.
        transport
            .fetch_batch(request)
            .await
            .map_or_else(|_| Ok(None), |frame| Ok(Some(frame.frontier().1)))
    }
}

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
#[path = "../tests/unit/authority_transfer_tests.rs"]
mod tests;
