use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::lifecycle::Lifecycle;
use crate::raft::log_store::RaftLogStoreAdapter;
use crate::raft::network::{PeerRaftNetworkFactory, RaftRpc, RaftRpcClient};
use crate::raft::persistence::RaftLogStore;
use crate::raft::{OwnershipResponse, OwnershipStateMachine, PeryxNode, RaftConfig, RaftNode, TypeConfig};
use crate::{
    Admission, AssignmentCause, AuthorityEpoch, AuthorityKey, ControlRejection, ControlResolution, DatacenterId,
    OwnershipCommand, OwnershipEffect, Rejection, control_outcome,
};
use anyhow::{Context as _, bail};
use openraft::error::{CheckIsLeaderError, ClientWriteError, RaftError};
use openraft::raft::ClientWriteResponse;
use openraft::{LogId, StoredMembership};
use peryx_core::Clock;
use peryx_ha::{
    AUTHORITY_WRITE_LEASE_SECS, AuthorityWriteLease, ClusterStatus, CommandOutcome, CommandReceipt, ControlCommand,
    ControlCommit, ControlError, HomeClaim, MemberEndpoint, MembershipControl, OwnershipAuthority, OwnershipError,
    SINGLETON_LEASE_SECS, SingletonAcquisition, SingletonLease, SingletonRelease, SingletonRenewal, TransferOutcome,
};

type VoterId = u64;

/// A peer RPC exceeding this deadline counts as a retryable loss.
const PEER_RPC_TIMEOUT: Duration = Duration::from_secs(5);
const MEMBERSHIP_PUBLICATION_TIMEOUT: Duration = Duration::from_secs(5);
const LEARNER_CATCH_UP_TIMEOUT: Duration = Duration::from_secs(30);

pub struct ConsensusPlan {
    pub(super) local: VoterId,
    pub(super) home: DatacenterId,
    /// Restricts group initialization to the designated writer.
    pub(super) seed: bool,
    pub(super) roster: BTreeMap<VoterId, PeryxNode>,
    pub(super) log_path: PathBuf,
    pub(super) group: String,
    pub(super) token: String,
}

pub struct StartedRaft {
    node: RaftNode,
    executor: RaftExecutor,
}

impl StartedRaft {
    const fn new(node: RaftNode, executor: RaftExecutor) -> Self {
        Self { node, executor }
    }

    pub(crate) const fn node(&self) -> &RaftNode {
        &self.node
    }

    pub(crate) fn commit(self) -> (RaftNode, RaftExecutor) {
        (self.node, self.executor)
    }
}

impl std::ops::Deref for StartedRaft {
    type Target = RaftNode;

    fn deref(&self) -> &Self::Target {
        self.node()
    }
}

struct RaftStartup {
    ready: tokio::sync::oneshot::Receiver<anyhow::Result<RaftNode>>,
    executor: RaftExecutor,
}

impl RaftStartup {
    async fn complete(self) -> anyhow::Result<StartedRaft> {
        match self
            .ready
            .await
            .context("the ownership consensus startup thread stopped before reporting readiness")?
        {
            Ok(node) => Ok(StartedRaft::new(node, self.executor)),
            Err(error) => {
                self.executor.shutdown();
                Err(error)
            }
        }
    }
}

pub struct RaftExecutor {
    state: Option<(
        tokio_util::sync::CancellationToken,
        std::thread::JoinHandle<anyhow::Result<()>>,
    )>,
}

impl RaftExecutor {
    pub const fn new(
        cancellation: tokio_util::sync::CancellationToken,
        thread: std::thread::JoinHandle<anyhow::Result<()>>,
    ) -> Self {
        Self {
            state: Some((cancellation, thread)),
        }
    }

    pub(crate) fn cancel(&self) {
        if let Some((cancellation, _)) = &self.state {
            cancellation.cancel();
        }
    }

    pub(crate) fn shutdown(mut self) {
        let (cancellation, thread) = self.state.take().expect("raft executor exists until shutdown");
        cancellation.cancel();
        spawn_raft_reaper(thread);
    }

    pub(crate) fn shutdown_and_join(mut self) -> anyhow::Result<()> {
        let (cancellation, thread) = self.state.take().expect("raft executor exists until shutdown");
        cancellation.cancel();
        join_raft_thread(thread)
    }
}

impl Drop for RaftExecutor {
    fn drop(&mut self) {
        if let Some((cancellation, thread)) = self.state.take() {
            cancellation.cancel();
            spawn_raft_reaper(thread);
        }
    }
}

fn join_raft_thread(thread: std::thread::JoinHandle<anyhow::Result<()>>) -> anyhow::Result<()> {
    thread
        .join()
        .unwrap_or(Err(anyhow::anyhow!("ownership consensus thread panicked")))
}

fn spawn_raft_reaper(thread: std::thread::JoinHandle<anyhow::Result<()>>) {
    drop(crate::service_assembly::reap_process_resource(
        "ownership consensus",
        move || join_raft_thread(thread),
    ));
}

impl ConsensusPlan {
    #[must_use]
    pub fn home(&self) -> DatacenterId {
        self.home.clone()
    }

    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The voter ID inbound RPCs must address for this process to answer them.
    #[must_use]
    pub const fn local_voter(&self) -> VoterId {
        self.local
    }

    /// # Errors
    /// Returns an error for an invalid member address or voter ID collision.
    pub fn new(
        local_datacenter: String,
        seed: bool,
        members: &[ConsensusMember],
        log_path: PathBuf,
        group: String,
        token: String,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            local: voter_id(&local_datacenter),
            home: DatacenterId(local_datacenter),
            seed,
            roster: build_roster(members)?,
            log_path,
            group,
            token,
        })
    }

    /// # Errors
    /// Returns an error if startup cannot open the log directory or store, start the node, or bootstrap
    /// an inconsistent roster.
    pub async fn ignite(&self) -> anyhow::Result<StartedRaft> {
        self.ignite_supervised(None).await
    }

    pub(crate) async fn ignite_with_lifecycle(&self, lifecycle: Lifecycle) -> anyhow::Result<StartedRaft> {
        self.ignite_supervised(Some(lifecycle)).await
    }

    async fn ignite_supervised(&self, lifecycle: Option<Lifecycle>) -> anyhow::Result<StartedRaft> {
        let parent = self.log_path.parent().context("the consensus log path has no parent")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create the consensus log directory {}", parent.display()))?;
        let log_path = self.log_path.clone();
        let local = self.local;
        let group = self.group.clone();
        let token = self.token.clone();
        let seed = self.seed;
        let roster = self.roster.clone();
        let cancellation = tokio_util::sync::CancellationToken::new();
        let thread_cancellation = cancellation.clone();
        let runtime_cancellation = cancellation.clone();
        let supervision = lifecycle;
        let (ready, readiness) = tokio::sync::oneshot::channel();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("peryx-raft-worker")
            .enable_all()
            .build()
            .context("build the ownership consensus runtime")?;
        let thread = std::thread::Builder::new()
            .name("peryx-raft".to_owned())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    runtime.block_on(async move {
                        let startup = async move {
                            let store = RaftLogStore::open(&log_path)
                                .with_context(|| format!("open the consensus log store at {}", log_path.display()))?;
                            let state_machine = OwnershipStateMachine::with_snapshot_store(store.clone())
                                .context("reload the persisted ownership snapshot")?;
                            let node = RaftNode::start(
                                local,
                                RaftConfig::default(),
                                group,
                                PeerRaftNetworkFactory::new(token, PEER_RPC_TIMEOUT),
                                RaftLogStoreAdapter::new(store),
                                state_machine,
                            )
                            .await
                            .context("start the ownership consensus node")?;
                            if seed {
                                let expected_voters = roster.len();
                                node.bootstrap(roster)
                                    .await
                                    .context("bootstrap the ownership consensus group")?;
                                wait_for_membership_publication(
                                    node.metrics(),
                                    expected_voters,
                                    MEMBERSHIP_PUBLICATION_TIMEOUT,
                                )
                                .await?;
                            }
                            anyhow::Ok(node)
                        };
                        let node = tokio::select! {
                            () = runtime_cancellation.cancelled() => return Ok(()),
                            result = startup => match result {
                                Ok(node) => node,
                                Err(error) => {
                                    let _ = ready.send(Err(error));
                                    return Ok(());
                                }
                            },
                        };
                        let _ = ready.send(Ok(node.clone()));
                        runtime_cancellation.cancelled().await;
                        node.raft()
                            .shutdown()
                            .await
                            .context("stop the ownership consensus node")
                    })
                }));
                let result = result.unwrap_or(Err(anyhow::anyhow!("ownership consensus thread panicked")));
                if let Some(supervision) = supervision.as_ref() {
                    report_raft_exit(supervision, &thread_cancellation, &result);
                }
                result
            })
            .context("spawn the ownership consensus thread")?;
        RaftStartup {
            ready: readiness,
            executor: RaftExecutor::new(cancellation, thread),
        }
        .complete()
        .await
    }
}

pub async fn wait_for_membership_publication(
    mut metrics: tokio::sync::watch::Receiver<openraft::RaftMetrics<VoterId, PeryxNode>>,
    expected_voters: usize,
    timeout: Duration,
) -> anyhow::Result<()> {
    tokio::time::timeout(
        timeout,
        metrics.wait_for(|metrics| metrics.membership_config.voter_ids().count() == expected_voters),
    )
    .await
    .context("time out waiting for ownership consensus membership publication")?
    .context("ownership consensus metrics closed before publishing bootstrap membership")?;
    Ok(())
}

pub fn report_raft_exit(
    supervision: &Lifecycle,
    cancellation: &tokio_util::sync::CancellationToken,
    result: &anyhow::Result<()>,
) {
    if cancellation.is_cancelled() {
        return;
    }
    let failure = match result {
        Ok(()) => "ownership consensus executor stopped unexpectedly".to_owned(),
        Err(error) => format!("ownership consensus executor failed: {error:#}"),
    };
    supervision.fail(failure);
}

pub struct OwnershipGroup {
    node: RaftNode,
    home: DatacenterId,
    peer_token: Option<String>,
    clock: Clock,
}

pub struct OwnershipHandle {
    group: std::sync::Weak<OwnershipGroup>,
}

impl OwnershipHandle {
    pub(crate) fn new(group: &Arc<OwnershipGroup>) -> Self {
        Self {
            group: Arc::downgrade(group),
        }
    }
}

impl OwnershipGroup {
    #[must_use]
    pub fn new(node: RaftNode, home: DatacenterId) -> Self {
        Self {
            node,
            home,
            peer_token: None,
            clock: Arc::new(unix_now),
        }
    }

    #[must_use]
    pub fn with_peer_forwarding(mut self, token: impl Into<String>) -> Self {
        self.peer_token = Some(token.into());
        self
    }

    #[must_use]
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
}

#[async_trait::async_trait]
impl OwnershipAuthority for OwnershipHandle {
    async fn committed_epoch(&self, authority: &str) -> u64 {
        match self.group.upgrade() {
            Some(group) => OwnershipAuthority::committed_epoch(group.as_ref(), authority).await,
            None => 0,
        }
    }

    async fn admit_epoch(&self, authority: &str, presented: u64) -> bool {
        match self.group.upgrade() {
            Some(group) => OwnershipAuthority::admit_epoch(group.as_ref(), authority, presented).await,
            None => false,
        }
    }

    async fn begin_epoch_write(
        &self,
        authority: &str,
        presented: u64,
    ) -> Result<Option<AuthorityWriteLease>, OwnershipError> {
        let group = self
            .group
            .upgrade()
            .ok_or_else(|| OwnershipError::Unavailable("ownership consensus stopped".to_owned()))?;
        OwnershipAuthority::begin_epoch_write(group.as_ref(), authority, presented).await
    }

    async fn finish_epoch_write(&self, lease: &AuthorityWriteLease) -> Result<(), OwnershipError> {
        let group = self
            .group
            .upgrade()
            .ok_or_else(|| OwnershipError::Unavailable("ownership consensus stopped".to_owned()))?;
        OwnershipAuthority::finish_epoch_write(group.as_ref(), lease).await
    }

    async fn acquire_singleton_lease(&self, job: &str, holder: &str) -> Result<SingletonAcquisition, OwnershipError> {
        let group = self
            .group
            .upgrade()
            .ok_or_else(|| OwnershipError::Unavailable("ownership consensus stopped".to_owned()))?;
        OwnershipAuthority::acquire_singleton_lease(group.as_ref(), job, holder).await
    }

    async fn renew_singleton_lease(&self, lease: &SingletonLease) -> Result<SingletonRenewal, OwnershipError> {
        let group = self
            .group
            .upgrade()
            .ok_or_else(|| OwnershipError::Unavailable("ownership consensus stopped".to_owned()))?;
        OwnershipAuthority::renew_singleton_lease(group.as_ref(), lease).await
    }

    async fn release_singleton_lease(&self, lease: &SingletonLease) -> Result<SingletonRelease, OwnershipError> {
        let group = self
            .group
            .upgrade()
            .ok_or_else(|| OwnershipError::Unavailable("ownership consensus stopped".to_owned()))?;
        OwnershipAuthority::release_singleton_lease(group.as_ref(), lease).await
    }

    async fn claim_home(&self, authority: &str) -> Result<HomeClaim, OwnershipError> {
        let group = self
            .group
            .upgrade()
            .ok_or_else(|| OwnershipError::Unavailable("ownership consensus stopped".to_owned()))?;
        OwnershipAuthority::claim_home(group.as_ref(), authority).await
    }

    async fn transfer_home(&self, authority: &str, new_home: &str) -> Result<Option<TransferOutcome>, OwnershipError> {
        let group = self
            .group
            .upgrade()
            .ok_or_else(|| OwnershipError::Unavailable("ownership consensus stopped".to_owned()))?;
        OwnershipAuthority::transfer_home(group.as_ref(), authority, new_home).await
    }

    fn cluster_status(&self) -> ClusterStatus {
        self.group.upgrade().map_or(
            ClusterStatus {
                leader: None,
                term: 0,
                voters: Vec::new(),
            },
            |group| OwnershipAuthority::cluster_status(group.as_ref()),
        )
    }
}

#[async_trait::async_trait]
impl MembershipControl for OwnershipHandle {
    async fn submit(&self, key: Option<&str>, command: ControlCommand) -> Result<ControlCommit, ControlError> {
        let group = self
            .group
            .upgrade()
            .ok_or_else(|| ControlError::Unavailable("ownership consensus stopped".to_owned()))?;
        MembershipControl::submit(group.as_ref(), key, command).await
    }
}

#[async_trait::async_trait]
impl OwnershipAuthority for OwnershipGroup {
    async fn committed_epoch(&self, authority: &str) -> u64 {
        self.node
            .state_machine()
            .epoch_of(&AuthorityKey(authority.to_owned()))
            .await
            .0
    }

    async fn admit_epoch(&self, authority: &str, presented: u64) -> bool {
        let admission = self
            .node
            .state_machine()
            .admit(&AuthorityKey(authority.to_owned()), AuthorityEpoch(presented))
            .await;
        matches!(admission, Admission::Admit)
    }

    async fn begin_epoch_write(
        &self,
        authority: &str,
        presented: u64,
    ) -> Result<Option<AuthorityWriteLease>, OwnershipError> {
        let issued_at_unix = (self.clock)();
        let command = OwnershipCommand::BeginEpochWrite {
            authority: AuthorityKey(authority.to_owned()),
            epoch: AuthorityEpoch(presented),
            id: uuid::Uuid::new_v4().to_string(),
            issued_at_unix,
            expires_at_unix: issued_at_unix.saturating_add(AUTHORITY_WRITE_LEASE_SECS),
        };
        match self.submit_command(command).await?.data {
            OwnershipResponse::Applied(OwnershipEffect::WriteLeased {
                epoch,
                id,
                expires_at_unix,
            }) => Ok(Some(AuthorityWriteLease {
                authority: authority.to_owned(),
                epoch: epoch.0,
                id,
                expires_at_unix,
            })),
            OwnershipResponse::Applied(OwnershipEffect::Rejected(
                Rejection::EpochMismatch | Rejection::NotAssigned,
            )) => Ok(None),
            response => Err(OwnershipError::Unavailable(format!(
                "ownership write lease returned {response:?}"
            ))),
        }
    }

    async fn finish_epoch_write(&self, lease: &AuthorityWriteLease) -> Result<(), OwnershipError> {
        let command = OwnershipCommand::FinishEpochWrite {
            authority: AuthorityKey(lease.authority.clone()),
            epoch: AuthorityEpoch(lease.epoch),
            id: lease.id.clone(),
        };
        match self.submit_command(command).await?.data {
            OwnershipResponse::Applied(OwnershipEffect::WriteFinished) => Ok(()),
            response => Err(OwnershipError::Unavailable(format!(
                "ownership write release returned {response:?}"
            ))),
        }
    }

    async fn acquire_singleton_lease(&self, job: &str, holder: &str) -> Result<SingletonAcquisition, OwnershipError> {
        let now_unix = (self.clock)();
        let command = OwnershipCommand::AcquireSingletonLease {
            job: job.to_owned(),
            holder: holder.to_owned(),
            now_unix,
            expires_at_unix: now_unix.saturating_add(SINGLETON_LEASE_SECS),
        };
        match self.submit_command(command).await?.data {
            OwnershipResponse::Applied(OwnershipEffect::SingletonAcquired {
                holder,
                term,
                generation,
                expires_at_unix,
            }) => Ok(SingletonAcquisition::Acquired(SingletonLease {
                job: job.to_owned(),
                holder,
                term,
                generation,
                expires_at_unix,
            })),
            OwnershipResponse::Applied(OwnershipEffect::SingletonHeld { holder }) => {
                Ok(SingletonAcquisition::Held { holder })
            }
            response => Err(OwnershipError::Unavailable(format!(
                "singleton claim returned {response:?}"
            ))),
        }
    }

    async fn renew_singleton_lease(&self, lease: &SingletonLease) -> Result<SingletonRenewal, OwnershipError> {
        let now_unix = (self.clock)();
        let command = OwnershipCommand::RenewSingletonLease {
            job: lease.job.clone(),
            holder: lease.holder.clone(),
            term: lease.term,
            generation: lease.generation,
            now_unix,
            expires_at_unix: now_unix.saturating_add(SINGLETON_LEASE_SECS),
        };
        match self.submit_command(command).await?.data {
            OwnershipResponse::Applied(OwnershipEffect::SingletonRenewed { expires_at_unix }) => {
                Ok(SingletonRenewal::Renewed(SingletonLease {
                    expires_at_unix,
                    ..lease.clone()
                }))
            }
            OwnershipResponse::Applied(OwnershipEffect::Rejected(Rejection::SingletonLost)) => {
                Ok(SingletonRenewal::Lost)
            }
            response => Err(OwnershipError::Unavailable(format!(
                "singleton renewal returned {response:?}"
            ))),
        }
    }

    async fn release_singleton_lease(&self, lease: &SingletonLease) -> Result<SingletonRelease, OwnershipError> {
        let command = OwnershipCommand::ReleaseSingletonLease {
            job: lease.job.clone(),
            holder: lease.holder.clone(),
            term: lease.term,
            generation: lease.generation,
            now_unix: (self.clock)(),
        };
        match self.submit_command(command).await?.data {
            OwnershipResponse::Applied(OwnershipEffect::SingletonReleased) => Ok(SingletonRelease::Released),
            OwnershipResponse::Applied(OwnershipEffect::Rejected(Rejection::SingletonLost)) => {
                Ok(SingletonRelease::Lost)
            }
            response => Err(OwnershipError::Unavailable(format!(
                "singleton release returned {response:?}"
            ))),
        }
    }

    async fn claim_home(&self, authority: &str) -> Result<HomeClaim, OwnershipError> {
        let authority = AuthorityKey(authority.to_owned());
        if self.node.state_machine().home_of(&authority).await.is_some() {
            match self.node.raft().ensure_linearizable().await {
                Ok(_) => {
                    if let Some((home, epoch)) = self.node.state_machine().home_claim(&authority).await {
                        return Ok(HomeClaim {
                            home: home.0,
                            epoch: epoch.0,
                        });
                    }
                }
                Err(RaftError::APIError(CheckIsLeaderError::ForwardToLeader(_))) if self.peer_token.is_some() => {}
                Err(RaftError::APIError(CheckIsLeaderError::ForwardToLeader(forward))) => {
                    return Err(OwnershipError::NotLeader {
                        leader: forward.leader_node.map(|node| node.endpoint),
                    });
                }
                Err(error) => return Err(OwnershipError::Unavailable(error.to_string())),
            }
        }
        let command = OwnershipCommand::AssignHome {
            authority: authority.clone(),
            home: self.home.clone(),
            cause: AssignmentCause::FirstPublish,
        };
        match self.submit_command(command).await?.data {
            OwnershipResponse::Applied(
                OwnershipEffect::Assigned { home, epoch } | OwnershipEffect::AlreadyAssigned { home, epoch },
            ) => Ok(HomeClaim {
                home: home.0,
                epoch: epoch.0,
            }),
            response => Err(OwnershipError::Unavailable(format!(
                "ownership assignment returned {response:?}"
            ))),
        }
    }

    async fn transfer_home(&self, authority: &str, new_home: &str) -> Result<Option<TransferOutcome>, OwnershipError> {
        let command = OwnershipCommand::RecordTransfer {
            authority: AuthorityKey(authority.to_owned()),
            new_home: DatacenterId(new_home.to_owned()),
            now_unix: (self.clock)(),
        };
        match self.submit_command(command).await?.data {
            // Committed rejections retain the existing home; `Transferred` records a move.
            OwnershipResponse::Applied(OwnershipEffect::Transferred { from, to, epoch }) => Ok(Some(TransferOutcome {
                from: from.0,
                to: to.0,
                epoch: epoch.0,
            })),
            OwnershipResponse::Applied(OwnershipEffect::Rejected(Rejection::WritesInFlight)) => Err(
                OwnershipError::Unavailable("authority transfer is blocked by a live write lease".to_owned()),
            ),
            _ => Ok(None),
        }
    }

    fn cluster_status(&self) -> ClusterStatus {
        let metrics = self.node.metrics().borrow().clone();
        let membership = &metrics.membership_config;
        let leader = metrics.current_leader.and_then(|leader| {
            membership
                .nodes()
                .find(|(id, _)| **id == leader)
                .map(|(_, node)| node.datacenter.0.clone())
        });
        ClusterStatus {
            leader,
            term: metrics.current_term,
            voters: voter_names(membership, &membership.voter_ids().collect()),
        }
    }
}

#[async_trait::async_trait]
impl MembershipControl for OwnershipGroup {
    /// A keyed command resolves against the replicated window first. Consensus applies a transfer or
    /// epoch advance and records its receipt in that one decision, so a replacement leader answering a
    /// retry reads the committed result instead of mutating again. A membership change belongs to a
    /// consensus decision that cannot carry the receipt, so its key is bound first and settled after. A
    /// failed attempt settles too, releasing its claim rather than stranding the key for the window.
    async fn submit(&self, key: Option<&str>, command: ControlCommand) -> Result<ControlCommit, ControlError> {
        let Some(key) = key else {
            return self.execute(command).await.map(ControlCommit::committed);
        };
        let attempt = OwnershipCommand::AttemptControl {
            key: key.to_owned(),
            command: command.clone(),
            now_unix: (self.clock)(),
        };
        match applied_resolution(self.submit_ownership_command(attempt).await?.data)? {
            ControlResolution::Committed(receipt) => Ok(ControlCommit::committed(receipt)),
            ControlResolution::Replayed(receipt) => Ok(ControlCommit::replayed(receipt)),
            ControlResolution::Rejected(rejection) => Err(reject_control(rejection)),
            ControlResolution::KeyReuse => Err(ControlError::KeyReuse),
            ControlResolution::Claimed => {
                let executed = self.execute(command.clone()).await;
                let now_unix = (self.clock)();
                let key = key.to_owned();
                // Both outcomes settle through the window, so a failed key resolves a retry by the
                // command it carries instead of by how long its claim has left to age out.
                let settlement = match &executed {
                    Ok(receipt) => OwnershipCommand::SettleControl {
                        key,
                        command,
                        receipt: receipt.clone(),
                        now_unix,
                    },
                    Err(_) => OwnershipCommand::ReleaseControl { key, command, now_unix },
                };
                let settled = self.submit_ownership_command(settlement).await;
                // An execution failure is what the caller must act on, so it outranks a settlement
                // that could not commit; the age-only prune stays the backstop for a claim left behind.
                executed?;
                applied_receipt(settled?.data).map(ControlCommit::committed)
            }
        }
    }
}

impl OwnershipGroup {
    async fn execute(&self, command: ControlCommand) -> Result<CommandReceipt, ControlError> {
        match command {
            ControlCommand::AddLearner { datacenter, address } => self.add_learner(&datacenter, address).await,
            ControlCommand::PromoteVoter { datacenter } => self.change_voters(Some(&datacenter), None).await,
            ControlCommand::RemoveVoter { datacenter } => self.change_voters(None, Some(&datacenter)).await,
            ControlCommand::ReplaceVoter {
                remove,
                datacenter,
                address,
            } => {
                self.add_learner(&datacenter, address).await?;
                self.change_voters(Some(&datacenter), Some(&remove)).await
            }
            ControlCommand::TransferAuthority { authority, new_home } => {
                self.submit_ownership(OwnershipCommand::RecordTransfer {
                    authority: AuthorityKey(authority),
                    new_home: DatacenterId(new_home),
                    now_unix: (self.clock)(),
                })
                .await
            }
            ControlCommand::AdvanceEpoch { authority } => {
                self.submit_ownership(OwnershipCommand::AdvanceAuthorityEpoch {
                    authority: AuthorityKey(authority),
                    now_unix: (self.clock)(),
                })
                .await
            }
            ControlCommand::ForgetAuthority { authority } => {
                self.submit_ownership(OwnershipCommand::ForgetAuthority {
                    authority: AuthorityKey(authority),
                    now_unix: (self.clock)(),
                })
                .await
            }
        }
    }

    async fn submit_command(
        &self,
        command: OwnershipCommand,
    ) -> Result<ClientWriteResponse<TypeConfig>, OwnershipError> {
        match self.node.raft().client_write(command.clone()).await {
            Ok(response) => Ok(response),
            Err(error) => {
                if !matches!(error, RaftError::APIError(ClientWriteError::ForwardToLeader(_))) {
                    return Err(map_ownership_write_error(error));
                }
                let Some(token) = self.peer_token.as_deref() else {
                    return Err(map_ownership_write_error(error));
                };
                let Some((id, target)) = self.node.forward_target(&error) else {
                    return Err(map_ownership_write_error(error));
                };
                let client = RaftRpcClient::new(id, &target.endpoint, token, PEER_RPC_TIMEOUT)
                    .expect("the replication token and peer endpoint were validated at startup");
                let response: Result<
                    ClientWriteResponse<TypeConfig>,
                    RaftError<VoterId, ClientWriteError<VoterId, PeryxNode>>,
                > = client
                    .send(RaftRpc::ClientWrite, &command)
                    .await
                    .map_err(|error| OwnershipError::Unavailable(error.to_string()))?;
                response.map_err(map_ownership_write_error)
            }
        }
    }

    /// Applies the configuration contract to a dynamic address so a learner cannot join on a form static
    /// membership would reject.
    async fn add_learner(&self, datacenter: &str, address: String) -> Result<CommandReceipt, ControlError> {
        let id = voter_id(datacenter);
        let node = PeryxNode {
            datacenter: DatacenterId(datacenter.to_owned()),
            endpoint: MemberEndpoint::parse(&address)
                .map_err(|error| ControlError::Invalid(error.to_string()))?
                .into_string(),
        };
        // Learners do not change either side of the audited voter transition.
        let metrics = self.node.metrics().borrow().clone();
        admit_member_identity(&metrics.membership_config, id, &node)?;
        let voters: BTreeSet<u64> = metrics.membership_config.voter_ids().collect();
        let voters = voter_names(&metrics.membership_config, &voters);
        match self.node.raft().add_learner(id, node, false).await {
            Ok(response) => Ok(committed_receipt(
                &response.log_id,
                CommandOutcome::Committed,
                voters.clone(),
                voters,
            )),
            Err(error) => Err(map_write_error(&error)),
        }
    }

    /// Reports an unchanged voter roster as [`CommandOutcome::NoChange`].
    async fn change_voters(&self, add: Option<&str>, remove: Option<&str>) -> Result<CommandReceipt, ControlError> {
        let metrics = self.node.metrics().borrow().clone();
        let current: BTreeSet<u64> = metrics.membership_config.voter_ids().collect();
        let planned = crate::control::plan_voter_roster(&current, add.map(voter_id), remove.map(voter_id));
        if let Some(datacenter) = add.filter(|datacenter| {
            let id = voter_id(datacenter);
            planned.contains(&id) && !current.contains(&id)
        }) {
            self.await_learner_catch_up(datacenter, &metrics.membership_config)
                .await?;
        }
        let outcome = if planned == current {
            CommandOutcome::NoChange
        } else {
            CommandOutcome::Committed
        };
        // A promoted learner has node data, so both audit rosters can use datacenter names.
        let old_voters = voter_names(&metrics.membership_config, &current);
        let new_voters = voter_names(&metrics.membership_config, &planned);
        match self.node.raft().change_membership(planned, false).await {
            Ok(response) => Ok(committed_receipt(&response.log_id, outcome, old_voters, new_voters)),
            Err(error) => Err(map_write_error(&error)),
        }
    }

    /// The first non-blocking learner add starts replication and returns before catch-up.
    async fn await_learner_catch_up(
        &self,
        datacenter: &str,
        membership: &StoredMembership<VoterId, PeryxNode>,
    ) -> Result<(), ControlError> {
        let id = voter_id(datacenter);
        let node = membership
            .nodes()
            .find_map(|(member, node)| (*member == id).then(|| node.clone()))
            .ok_or_else(|| {
                ControlError::Unavailable(format!("datacenter {datacenter:?} is not a registered learner"))
            })?;
        match tokio::time::timeout(LEARNER_CATCH_UP_TIMEOUT, self.node.raft().add_learner(id, node, true)).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(error)) => Err(map_write_error(&error)),
            Err(_) => Err(ControlError::Unavailable(format!(
                "datacenter {datacenter:?} did not catch up within {} seconds",
                LEARNER_CATCH_UP_TIMEOUT.as_secs()
            ))),
        }
    }

    /// Returns an unchanged transfer as a committed no-op so a retry receives a receipt.
    async fn submit_ownership(&self, command: OwnershipCommand) -> Result<CommandReceipt, ControlError> {
        let response = self.submit_ownership_command(command).await?;
        let outcome = applied_outcome(response.data)?;
        // Authority commands have no voter transition to audit.
        Ok(committed_receipt(&response.log_id, outcome, Vec::new(), Vec::new()))
    }

    async fn submit_ownership_command(
        &self,
        command: OwnershipCommand,
    ) -> Result<ClientWriteResponse<TypeConfig>, ControlError> {
        match self.submit_command(command).await {
            Ok(response) => Ok(response),
            Err(OwnershipError::NotLeader { leader }) => Err(ControlError::NotLeader { leader }),
            Err(error) => Err(ControlError::Unavailable(error.to_string())),
        }
    }
}

fn reject_control(rejection: ControlRejection) -> ControlError {
    match rejection {
        ControlRejection::NotAssigned => {
            ControlError::Invalid("the authority is not assigned a home to move or fence".to_owned())
        }
        ControlRejection::WritesInFlight => ControlError::Invalid("the authority has a live write lease".to_owned()),
    }
}

/// A normal ownership entry always reaches the state machine, so a non-mutating answer means the
/// committed entry and the response to it disagree.
pub fn applied_outcome(response: OwnershipResponse) -> Result<CommandOutcome, ControlError> {
    match response {
        OwnershipResponse::Applied(effect) => control_outcome(&effect).map_err(reject_control),
        OwnershipResponse::NonMutating => Err(unapplied("authority command")),
    }
}

pub fn applied_resolution(response: OwnershipResponse) -> Result<ControlResolution, ControlError> {
    match response {
        OwnershipResponse::Applied(OwnershipEffect::Control(resolution)) => Ok(resolution),
        _ => Err(unapplied("control claim")),
    }
}

pub fn applied_receipt(response: OwnershipResponse) -> Result<CommandReceipt, ControlError> {
    match response {
        OwnershipResponse::Applied(OwnershipEffect::ControlSettled(receipt)) => Ok(receipt),
        _ => Err(unapplied("control settlement")),
    }
}

fn unapplied(what: &str) -> ControlError {
    ControlError::Unavailable(format!("the {what} committed without applying"))
}

fn map_ownership_write_error(error: RaftError<VoterId, ClientWriteError<VoterId, PeryxNode>>) -> OwnershipError {
    match error {
        RaftError::APIError(ClientWriteError::ForwardToLeader(forward)) => OwnershipError::NotLeader {
            leader: forward.leader_node.map(|node| node.endpoint),
        },
        error => OwnershipError::Unavailable(error.to_string()),
    }
}

/// `OpenRaft` keeps the committed node entry when `add_learner` repeats an ID, so a request that
/// disagrees with committed metadata would commit nothing and still report success. Two IDs sharing one
/// endpoint is worse: the leader opens a replication stream per ID to the same process, and one process
/// then answers a vote under two voter identities.
fn admit_member_identity(
    membership: &StoredMembership<VoterId, PeryxNode>,
    id: VoterId,
    node: &PeryxNode,
) -> Result<(), ControlError> {
    for (member_id, member) in membership.nodes() {
        if *member_id == id && member != node {
            return Err(ControlError::Invalid(format!(
                "datacenter {:?} maps to voter id {id}, which datacenter {:?} already holds at {}",
                node.datacenter.0, member.datacenter.0, member.endpoint
            )));
        }
        if *member_id != id && member.endpoint == node.endpoint {
            return Err(ControlError::Invalid(format!(
                "endpoint {} already belongs to datacenter {:?}; remove that member before reusing it",
                node.endpoint, member.datacenter.0
            )));
        }
    }
    Ok(())
}

const fn committed_receipt(
    log_id: &LogId<u64>,
    outcome: CommandOutcome,
    old_voters: Vec<String>,
    new_voters: Vec<String>,
) -> CommandReceipt {
    CommandReceipt {
        term: log_id.leader_id.term,
        index: log_id.index,
        outcome,
        old_voters,
        new_voters,
    }
}

fn voter_names(membership: &StoredMembership<u64, PeryxNode>, voter_ids: &BTreeSet<u64>) -> Vec<String> {
    membership
        .nodes()
        .filter(|(id, _)| voter_ids.contains(*id))
        .map(|(_, node)| node.datacenter.0.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Preserves leader forwarding; maps other write failures to unavailable.
pub fn map_write_error(error: &RaftError<u64, ClientWriteError<u64, PeryxNode>>) -> ControlError {
    match error {
        RaftError::APIError(ClientWriteError::ForwardToLeader(forward)) => ControlError::NotLeader {
            leader: forward.leader_node.as_ref().map(|node| node.endpoint.clone()),
        },
        other => ControlError::Unavailable(other.to_string()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsensusMember {
    pub datacenter: String,
    pub address: String,
}

pub fn build_roster(members: &[ConsensusMember]) -> anyhow::Result<BTreeMap<VoterId, PeryxNode>> {
    let mut roster = BTreeMap::new();
    let mut endpoints = HashMap::new();
    for member in members {
        let node = PeryxNode {
            datacenter: DatacenterId(member.datacenter.clone()),
            endpoint: MemberEndpoint::parse(&member.address)?.into_string(),
        };
        if let Some(owner) = endpoints.insert(node.endpoint.clone(), member.datacenter.clone()) {
            bail!(
                "datacenters {:?} and {:?} resolve to the same consensus endpoint {}",
                owner,
                member.datacenter,
                node.endpoint
            );
        }
        if let Some(existing) = roster.insert(voter_id(&member.datacenter), node) {
            bail!(
                "datacenter {:?} collides with {:?} on the same consensus voter id",
                member.datacenter,
                existing.datacenter.0
            );
        }
    }
    Ok(roster)
}

/// Uses FNV-1a so each node and toolchain derives the same voter ID; the standard hasher has no stable
/// output contract.
pub fn voter_id(datacenter: &str) -> VoterId {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in datacenter.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}
