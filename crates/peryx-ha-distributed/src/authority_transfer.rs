//! Consensus receives a move after the target reaches the source barrier. Cancellation and commit
//! resolve against the same plan, preventing a cancelled move from reaching consensus.

use std::collections::{HashMap, VecDeque};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::{
    AuthorityEpoch, BatchRequest, DEFAULT_TRANSFER_LIMITS, HttpPeerTransport, PeerTransport, TransferAudit,
    TransferError, TransferPhase, TransferPlan, TransferRequest,
};
use peryx_ha::TransferAudit as StoredTransferAudit;
use peryx_ha::{
    ControlCommand, ControlError, OwnershipAuthority, OwnershipError, PendingTransferAudit, TransferIntent,
};
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

/// Reads and clears audit facts from the replicated ownership state.
#[async_trait::async_trait]
pub trait TransferAuditOutbox: Send + Sync {
    /// # Errors
    /// Returns an error when the replicated facts cannot be read.
    async fn pending_transfer_audits(&self) -> Result<Vec<PendingTransferAudit>, OwnershipError>;

    /// # Errors
    /// Returns an error when the clearing decision cannot commit.
    async fn complete_transfer_audit(&self, id: &str) -> Result<(), OwnershipError>;
}

/// Answers whether an authority already moved, from state that outlives both the coordinator's
/// retention window and the process, so a cancellation after a commit is never decided by an
/// evictable entry.
#[async_trait::async_trait]
pub trait CommittedTransfers: Send + Sync {
    /// # Errors
    /// Returns an error when the durable record cannot be read.
    async fn committed(&self, authority: &str) -> anyhow::Result<bool>;
}

#[async_trait::async_trait]
impl CommittedTransfers for MetaStore {
    async fn committed(&self, authority: &str) -> anyhow::Result<bool> {
        Ok(!self.transfer_audits(authority)?.is_empty())
    }
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
    #[error("transfer {id} committed but its audit projection is pending: {source}")]
    ProjectionPending {
        id: String,
        #[source]
        source: MetaError,
    },
    #[error("transfer {id} committed but its audit projection state is unavailable: {source}")]
    ProjectionStateUnavailable {
        id: String,
        #[source]
        source: OwnershipError,
    },
    #[error("read the sealed transfer audits: {0}")]
    Recover(#[source] OwnershipError),
    #[error("transfer {0} committed but sealed no recoverable audit")]
    Unsealed(String),
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

/// Commits a ready plan under its stable transfer identity, then projects the audit consensus sealed.
///
/// The identity deduplicates retries across leader loss, and the committing decision seals the whole
/// audit, so the epoch and log identity come from that decision rather than from a later read of
/// ownership state that a concurrent move could already have advanced. A store write that fails leaves
/// the sealed fact for [`recover_transfer_audits`].
///
/// # Errors
/// Returns [`TransferDriveError`] when the commit is refused, the plan refuses the commit, or storing
/// the audit fails.
pub async fn commit_transfer(
    plan: &mut TransferPlan,
    control: &dyn peryx_ha::ControlExecutor,
    outbox: &dyn TransferAuditOutbox,
    meta: &MetaStore,
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
                    intent: Some(TransferIntent {
                        source: request.source.0.clone(),
                        actor: request.actor.clone(),
                        reason: request.reason.clone(),
                        barrier: request.barrier,
                    }),
                },
            )
        }
        // A sealed plan returns its original audit without submitting another move; commit ignores these
        // placeholder values in this state.
        TransferPhase::Committed => return plan.commit(AuthorityEpoch(0), 0, 0).map_err(TransferDriveError::Plan),
        TransferPhase::Cancelled => return Err(TransferDriveError::Plan(TransferError::Cancelled)),
        TransferPhase::AwaitingCatchUp => return Err(TransferDriveError::Plan(TransferError::BarrierNotMet)),
    };
    let id = plan.request().id.clone();
    let receipt = control
        .execute(&actor, Some(&id), command)
        .await
        .map_err(TransferDriveError::Commit)?;
    let sealed = match outbox
        .pending_transfer_audits()
        .await
        .map_err(|source| TransferDriveError::ProjectionStateUnavailable { id: id.clone(), source })?
        .into_iter()
        .find(|fact| fact.id == id)
        .map(|fact| fact.audit)
    {
        Some(sealed) => sealed,
        // A cleared fact has already reached the audit store.
        None => projected(meta, &plan.request().authority.0, receipt.index)
            .map_err(|source| TransferDriveError::ProjectionPending { id: id.clone(), source })?
            .ok_or_else(|| TransferDriveError::Unsealed(id.clone()))?,
    };
    let audit = plan
        .commit(AuthorityEpoch(sealed.epoch), sealed.commit_term, sealed.commit_index)
        .map_err(TransferDriveError::Plan)?;
    project(outbox, meta, &id, &sealed)
        .await
        .map_err(|source| TransferDriveError::ProjectionPending { id, source })?;
    Ok(audit)
}

/// Stores audits left by an interrupted projection, then clears their replicated facts.
///
/// # Errors
/// Returns [`TransferDriveError`] when the sealed facts cannot be read or an audit cannot be stored.
pub async fn recover_transfer_audits(
    outbox: &dyn TransferAuditOutbox,
    meta: &MetaStore,
) -> Result<usize, TransferDriveError> {
    let pending = outbox
        .pending_transfer_audits()
        .await
        .map_err(TransferDriveError::Recover)?;
    let recovered = pending.len();
    for fact in pending {
        project(outbox, meta, &fact.id, &fact.audit)
            .await
            .map_err(TransferDriveError::Persist)?;
    }
    Ok(recovered)
}

/// Clears the fact after the store transaction commits. A crash between the two repeats an idempotent
/// write keyed by authority and commit index.
async fn project(
    outbox: &dyn TransferAuditOutbox,
    meta: &MetaStore,
    id: &str,
    audit: &StoredTransferAudit,
) -> Result<(), MetaError> {
    meta.record_transfer_audit(audit)?;
    if let Err(error) = outbox.complete_transfer_audit(id).await {
        tracing::warn!(transfer = id, %error, "clearing a projected transfer audit failed");
    }
    Ok(())
}

fn projected(meta: &MetaStore, authority: &str, index: u64) -> Result<Option<StoredTransferAudit>, MetaError> {
    Ok(meta
        .transfer_audits(authority)?
        .into_iter()
        .find(|audit| audit.commit_index == index))
}

#[async_trait::async_trait]
impl TransferAuditOutbox for Arc<dyn OwnershipAuthority> {
    async fn pending_transfer_audits(&self) -> Result<Vec<PendingTransferAudit>, OwnershipError> {
        OwnershipAuthority::pending_transfer_audits(self.as_ref()).await
    }

    async fn complete_transfer_audit(&self, id: &str) -> Result<(), OwnershipError> {
        OwnershipAuthority::complete_transfer_audit(self.as_ref(), id).await
    }
}

const DEFAULT_POLL: Duration = Duration::from_secs(2);

const DEFAULT_BUDGET: u32 = 150;

const DEFAULT_RETAINED: usize = 256;

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
    #[error("read the durable transfer record for {0}: {1}")]
    Durable(String, #[source] anyhow::Error),
}

/// The plans a cancellation can resolve against: the transfers still running, and a bounded window of
/// the authorities whose most recent transfer resolved without committing.
#[derive(Default)]
struct Registry {
    running: HashMap<String, Arc<tokio::sync::Mutex<TransferPlan>>>,
    abandoned: VecDeque<String>,
}

impl Registry {
    /// A later transfer supersedes the earlier outcome, so the window never answers for a plan the
    /// authority has already moved past.
    fn retire(&mut self, authority: &str, abandoned: bool, retained: usize) {
        self.running.remove(authority);
        self.abandoned.retain(|name| name != authority);
        if abandoned {
            self.abandoned.push_back(authority.to_owned());
            while self.abandoned.len() > retained {
                self.abandoned.pop_front();
            }
        }
    }
}

enum Registration {
    Running(Arc<tokio::sync::Mutex<TransferPlan>>),
    Abandoned,
    Missing,
}

/// Runs one transfer per authority. A resolved plan leaves the registry, so retained state is the
/// transfers in flight plus a fixed abandonment window rather than one plan per authority ever moved.
pub struct TransferCoordinator {
    frontier: Arc<dyn FrontierSource>,
    poll: Duration,
    budget: u32,
    retained: usize,
    registry: Mutex<Registry>,
}

impl TransferCoordinator {
    /// Retains the 256 most recently abandoned authorities.
    #[must_use]
    pub fn new(frontier: Arc<dyn FrontierSource>) -> Self {
        Self::with_schedule(frontier, DEFAULT_POLL, DEFAULT_BUDGET, DEFAULT_RETAINED)
    }

    /// `retained` bounds the abandonment window; a cancel for an authority evicted from it reads as
    /// unknown.
    #[must_use]
    pub fn with_schedule(frontier: Arc<dyn FrontierSource>, poll: Duration, budget: u32, retained: usize) -> Self {
        Self {
            frontier,
            poll,
            budget,
            retained,
            registry: Mutex::new(Registry::default()),
        }
    }

    /// Runs one transfer per authority. The plan is registered until its drive resolves, then retired.
    ///
    /// # Errors
    /// Returns [`Busy`](TransferRunError::Busy) when a transfer for the authority is already running,
    /// [`BarrierNotReached`](TransferRunError::BarrierNotReached) when the budget runs out before the
    /// target catches up, or [`Drive`](TransferRunError::Drive) when recovering an earlier projection,
    /// committing, or storing the move fails.
    pub async fn run(
        &self,
        request: TransferRequest,
        control: &dyn peryx_ha::ControlExecutor,
        outbox: &dyn TransferAuditOutbox,
        meta: &MetaStore,
    ) -> Result<TransferAudit, TransferRunError> {
        recover_transfer_audits(outbox, meta).await?;
        let authority = request.authority.0.clone();
        let plan = {
            // A panic cannot corrupt this lookup table, so recover its poisoned guard.
            let mut registry = self.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if registry.running.contains_key(&authority) {
                return Err(TransferRunError::Busy(authority));
            }
            let plan = Arc::new(tokio::sync::Mutex::new(TransferPlan::plan(request)));
            registry.running.insert(authority.clone(), plan.clone());
            plan
        };
        let outcome = self.drive(&plan, control, outbox, meta).await;
        // A committed move is answered from its persisted audit, so only an abandonment needs a window.
        let abandoned = plan.lock().await.phase() != TransferPhase::Committed;
        self.retire(&authority, abandoned);
        outcome
    }

    fn retire(&self, authority: &str, abandoned: bool) {
        let mut registry = self.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.retire(authority, abandoned, self.retained);
    }

    fn registration(&self, authority: &str) -> Registration {
        let registry = self.registry.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        match registry.running.get(authority) {
            Some(plan) => Registration::Running(plan.clone()),
            None if registry.abandoned.iter().any(|name| name == authority) => Registration::Abandoned,
            None => Registration::Missing,
        }
    }

    /// A concurrent cancellation reaches `commit_transfer`, which rejects the cancelled plan.
    async fn drive(
        &self,
        plan: &tokio::sync::Mutex<TransferPlan>,
        control: &dyn peryx_ha::ControlExecutor,
        outbox: &dyn TransferAuditOutbox,
        meta: &MetaStore,
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
            return commit_transfer(&mut plan, control, outbox, meta)
                .await
                .map_err(TransferRunError::Drive);
        }
        Err(TransferRunError::BarrierNotReached)
    }

    /// A cancel of a move that already committed resolves against the persisted audit, so it answers
    /// the same after the abandonment window evicted the authority and after a restart.
    ///
    /// # Errors
    /// Returns [`Unknown`](TransferCancelError::Unknown) when no transfer is registered and none
    /// committed, [`AlreadyCommitted`](TransferCancelError::AlreadyCommitted) when the move already
    /// committed, or [`Durable`](TransferCancelError::Durable) when the persisted record is unreadable.
    pub async fn cancel(&self, authority: &str, committed: &dyn CommittedTransfers) -> Result<(), TransferCancelError> {
        match self.registration(authority) {
            // `TransferPlan::cancel` rejects committed plans.
            Registration::Running(plan) => plan
                .lock()
                .await
                .cancel()
                .map_err(|_| TransferCancelError::AlreadyCommitted(authority.to_owned())),
            Registration::Abandoned => Ok(()),
            Registration::Missing => match committed.committed(authority).await {
                Ok(true) => Err(TransferCancelError::AlreadyCommitted(authority.to_owned())),
                Ok(false) => Err(TransferCancelError::Unknown(authority.to_owned())),
                Err(error) => Err(TransferCancelError::Durable(authority.to_owned(), error)),
            },
        }
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

#[cfg(test)]
#[path = "../tests/unit/authority_transfer_tests.rs"]
mod tests;
