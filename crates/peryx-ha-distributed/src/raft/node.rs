//! Membership changes require operator action; liveness timeouts do not alter membership.

use std::collections::BTreeMap;

use std::sync::Arc;

use bytes::Bytes;
use openraft::async_runtime::WatchReceiver as _;
use openraft::errors::{ClientWriteError, Fatal, ForwardReason, InitializeError, RaftError};
use openraft::network::{NetSnapshot, RaftNetworkFactory};
use openraft::raft::{AppendEntriesRequest, AppendEntriesResponse, VoteRequest};
use openraft::storage::RaftLogStorage;
use openraft::{ConfigError, ReadPolicy};
use openraft_legacy::network_v1::{ChunkedSnapshotReceiver as _, InstallSnapshotRequest};
use serde::Serialize;

use crate::ownership::OwnershipCommand;
use crate::raft::config::RaftConfig;
use crate::raft::network::{RaftRpc, RaftRpcHandler, RaftRpcRejection};
use crate::raft::{
    MetricsReceiver, OwnershipResponse, OwnershipStateMachine, PeryxNode, Raft, SnapshotData, TypeConfig,
};
use peryx_core::Clock;
use peryx_ha::{AUTHORITY_WRITE_LEASE_SECS, SINGLETON_LEASE_SECS};

type NodeId = u64;

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("invalid raft configuration: {0}")]
    Config(#[from] Box<ConfigError>),
    #[error("raft node failed to start: {0}")]
    Fatal(Box<Fatal<TypeConfig>>),
}

/// Keeps a shared state-machine handle for reads after a linearizable barrier.
#[derive(Clone)]
pub struct RaftNode {
    raft: Raft,
    state_machine: OwnershipStateMachine,
}

impl RaftNode {
    /// Starts Raft runtime tasks without joining a cluster. Call [`bootstrap`](Self::bootstrap) on a seed
    /// or add the node through the current leader.
    ///
    /// # Errors
    /// [`StartError::Config`] when the tuning is invalid, or [`StartError::Fatal`] when `OpenRaft`
    /// cannot build the node.
    pub async fn start<LS, N>(
        id: NodeId,
        config: RaftConfig,
        cluster_name: impl Into<String>,
        network: N,
        log_store: LS,
        state_machine: OwnershipStateMachine,
    ) -> Result<Self, StartError>
    where
        LS: RaftLogStorage<TypeConfig>,
        N: RaftNetworkFactory<TypeConfig>,
        N::Network: NetSnapshot<TypeConfig, SnapshotData = SnapshotData>,
    {
        let config = std::sync::Arc::new(config.into_openraft(cluster_name)?);
        let raft = Raft::new(id, config, network, log_store, state_machine.clone())
            .await
            .map_err(|error| StartError::Fatal(Box::new(error)))?;
        Ok(Self { raft, state_machine })
    }

    /// Treats [`InitializeError::NotAllowed`] as success so startup may repeat initialization. Running
    /// clusters must change membership through the leader.
    ///
    /// # Errors
    /// Returns [`RaftError`] for initialization failures other than an initialized cluster.
    pub async fn bootstrap(
        &self,
        members: BTreeMap<NodeId, PeryxNode>,
    ) -> Result<(), Box<RaftError<TypeConfig, InitializeError<TypeConfig>>>> {
        match self.raft.initialize(members).await {
            Ok(()) | Err(RaftError::APIError(InitializeError::NotAllowed(_))) => Ok(()),
            Err(error) => Err(Box::new(error)),
        }
    }

    /// Followers return a forward-to-leader error containing the retry target.
    ///
    /// # Errors
    /// Returns [`RaftError`] when this node is not the leader or the write cannot commit.
    pub async fn submit(
        &self,
        command: OwnershipCommand,
    ) -> Result<OwnershipResponse, Box<RaftError<TypeConfig, ClientWriteError<TypeConfig>>>> {
        let response = self
            .with_write_lease(|| self.raft.client_write(command.clone()))
            .await?;
        Ok(response.data)
    }

    /// A leader refuses writes and membership changes until a quorum has acknowledged it within its
    /// lease, which a leader elected moments ago has not collected yet. A linearizable read collects
    /// it, so `write` runs once more after one; a leader cut off from its quorum still refuses.
    ///
    /// # Errors
    /// Returns what `write` returns, or the first refusal when the read cannot reach a quorum.
    pub async fn with_write_lease<T, F>(
        &self,
        write: impl Fn() -> F,
    ) -> Result<T, RaftError<TypeConfig, ClientWriteError<TypeConfig>>>
    where
        F: Future<Output = Result<T, RaftError<TypeConfig, ClientWriteError<TypeConfig>>>>,
    {
        let result = write().await;
        let Err(RaftError::APIError(ClientWriteError::ForwardToLeader(forward))) = &result else {
            return result;
        };
        if forward.reason != ForwardReason::LeaseExpired
            || self.raft.ensure_linearizable(ReadPolicy::ReadIndex).await.is_err()
        {
            return result;
        }
        write().await
    }

    /// Prefers the leader captured in the write error because metrics may lag during an election. Falls
    /// back to current metrics when the error has no leader. The voter ID travels with the address so a
    /// forwarded write names the process it expects to answer.
    #[must_use]
    pub fn forward_target(
        &self,
        error: &RaftError<TypeConfig, ClientWriteError<TypeConfig>>,
    ) -> Option<(NodeId, PeryxNode)> {
        if let RaftError::APIError(ClientWriteError::ForwardToLeader(forward)) = error
            && let (Some(id), Some(node)) = (forward.leader_id, &forward.leader_node)
        {
            return Some((id, node.clone()));
        }
        self.leader()
    }

    /// Returns `None` when metrics have no leader or membership lacks its node data.
    #[must_use]
    pub fn leader(&self) -> Option<(NodeId, PeryxNode)> {
        let metrics = self.raft.metrics().borrow_watched().clone();
        let leader = metrics.current_leader?;
        metrics
            .membership_config
            .nodes()
            .find(|(id, _)| **id == leader)
            .map(|(id, node)| (*id, node.clone()))
    }

    #[must_use]
    pub fn metrics(&self) -> MetricsReceiver {
        self.raft.metrics()
    }

    #[must_use]
    pub const fn raft(&self) -> &Raft {
        &self.raft
    }

    /// Read through this handle after a linearizable barrier on [`raft`](Self::raft).
    #[must_use]
    pub const fn state_machine(&self) -> &OwnershipStateMachine {
        &self.state_machine
    }

    /// Mount behind [`raft_rpc_router`](crate::raft::network::raft_rpc_router); without inbound RPCs the
    /// group cannot reach quorum.
    #[must_use]
    pub fn rpc_handler(&self) -> Arc<dyn RaftRpcHandler> {
        self.rpc_handler_with_clock(Arc::new(unix_now))
    }

    #[must_use]
    pub fn rpc_handler_with_clock(&self, clock: Clock) -> Arc<dyn RaftRpcHandler> {
        Arc::new(OwnershipRpcHandler {
            node: self.clone(),
            clock,
        })
    }
}

struct OwnershipRpcHandler {
    node: RaftNode,
    clock: Clock,
}

#[async_trait::async_trait]
impl RaftRpcHandler for OwnershipRpcHandler {
    async fn handle(&self, rpc: RaftRpc, body: Bytes) -> Result<Vec<u8>, RaftRpcRejection> {
        match rpc {
            RaftRpc::AppendEntries => {
                let request: AppendEntriesRequest<TypeConfig> = decode(&body)?;
                // Peers decode the v1 response shape, so the 0.10 result converts back to it.
                Ok(encode(
                    &self
                        .node
                        .raft
                        .append_entries(request)
                        .await
                        .map(AppendEntriesResponse::from),
                ))
            }
            RaftRpc::ClientWrite => {
                let request = stamp_leader_time(decode(&body)?, (self.clock)());
                let raft = &self.node.raft;
                Ok(encode(
                    &self.node.with_write_lease(|| raft.client_write(request.clone())).await,
                ))
            }
            RaftRpc::Vote => {
                let request: VoteRequest<TypeConfig> = decode(&body)?;
                Ok(encode(&self.node.raft.vote(request).await))
            }
            RaftRpc::InstallSnapshot => {
                let request: InstallSnapshotRequest<TypeConfig> = decode(&body)?;
                Ok(encode(&self.node.raft.install_snapshot(request).await))
            }
        }
    }
}

fn stamp_leader_time(command: OwnershipCommand, now_unix: i64) -> OwnershipCommand {
    match command {
        OwnershipCommand::BeginEpochWrite {
            authority, epoch, id, ..
        } => OwnershipCommand::BeginEpochWrite {
            authority,
            epoch,
            id,
            issued_at_unix: now_unix,
            expires_at_unix: now_unix.saturating_add(AUTHORITY_WRITE_LEASE_SECS),
        },
        OwnershipCommand::AdvanceAuthorityEpoch { authority, .. } => {
            OwnershipCommand::AdvanceAuthorityEpoch { authority, now_unix }
        }
        OwnershipCommand::RecordTransfer {
            authority, new_home, ..
        } => OwnershipCommand::RecordTransfer {
            authority,
            new_home,
            now_unix,
        },
        OwnershipCommand::ForgetAuthority { authority, .. } => {
            OwnershipCommand::ForgetAuthority { authority, now_unix }
        }
        OwnershipCommand::AcquireSingletonLease { job, holder, .. } => OwnershipCommand::AcquireSingletonLease {
            job,
            holder,
            now_unix,
            expires_at_unix: now_unix.saturating_add(SINGLETON_LEASE_SECS),
        },
        OwnershipCommand::RenewSingletonLease {
            job,
            holder,
            term,
            generation,
            ..
        } => OwnershipCommand::RenewSingletonLease {
            job,
            holder,
            term,
            generation,
            now_unix,
            expires_at_unix: now_unix.saturating_add(SINGLETON_LEASE_SECS),
        },
        OwnershipCommand::ReleaseSingletonLease {
            job,
            holder,
            term,
            generation,
            ..
        } => OwnershipCommand::ReleaseSingletonLease {
            job,
            holder,
            term,
            generation,
            now_unix,
        },
        command => command,
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
}

fn decode<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T, RaftRpcRejection> {
    serde_json::from_slice(body).map_err(|_| RaftRpcRejection::Malformed)
}

fn encode<T: Serialize>(response: &T) -> Vec<u8> {
    serde_json::to_vec(response).expect("a raft rpc response serializes to JSON")
}
