//! Membership changes require operator action; liveness timeouts do not alter membership.

use std::collections::BTreeMap;

use std::sync::Arc;

use bytes::Bytes;
use openraft::error::{ClientWriteError, Fatal, InitializeError, RaftError};
use openraft::raft::{AppendEntriesRequest, InstallSnapshotRequest, VoteRequest};
use openraft::storage::RaftLogStorage;
use openraft::{ConfigError, Raft, RaftMetrics, RaftNetworkFactory};
use serde::Serialize;
use tokio::sync::watch;

use crate::ownership::OwnershipCommand;
use crate::raft::config::RaftConfig;
use crate::raft::network::{RaftRpc, RaftRpcHandler, RaftRpcRejection};
use crate::raft::{OwnershipResponse, OwnershipStateMachine, PeryxNode, TypeConfig};

type NodeId = u64;

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("invalid raft configuration: {0}")]
    Config(#[from] Box<ConfigError>),
    #[error("raft node failed to start: {0}")]
    Fatal(Box<Fatal<NodeId>>),
}

/// Keeps a shared state-machine handle for reads after a linearizable barrier.
#[derive(Clone)]
pub struct RaftNode {
    raft: Raft<TypeConfig>,
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
    ) -> Result<(), RaftError<NodeId, InitializeError<NodeId, PeryxNode>>> {
        match self.raft.initialize(members).await {
            Ok(()) | Err(RaftError::APIError(InitializeError::NotAllowed(_))) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Followers return a forward-to-leader error containing the retry target.
    ///
    /// # Errors
    /// Returns [`RaftError`] when this node is not the leader or the write cannot commit.
    pub async fn submit(
        &self,
        command: OwnershipCommand,
    ) -> Result<OwnershipResponse, RaftError<NodeId, ClientWriteError<NodeId, PeryxNode>>> {
        let response = self.raft.client_write(command).await?;
        Ok(response.data)
    }

    /// Prefers the leader captured in the write error because metrics may lag during an election. Falls
    /// back to current metrics when the error has no leader.
    #[must_use]
    pub fn forward_target(&self, error: &RaftError<NodeId, ClientWriteError<NodeId, PeryxNode>>) -> Option<PeryxNode> {
        if let RaftError::APIError(ClientWriteError::ForwardToLeader(forward)) = error
            && let Some(node) = &forward.leader_node
        {
            return Some(node.clone());
        }
        self.leader()
    }

    /// Returns `None` when metrics have no leader or membership lacks its node data.
    #[must_use]
    pub fn leader(&self) -> Option<PeryxNode> {
        let metrics = self.raft.metrics().borrow().clone();
        let leader = metrics.current_leader?;
        metrics
            .membership_config
            .nodes()
            .find(|(id, _)| **id == leader)
            .map(|(_, node)| node.clone())
    }

    #[must_use]
    pub fn metrics(&self) -> watch::Receiver<RaftMetrics<NodeId, PeryxNode>> {
        self.raft.metrics()
    }

    #[must_use]
    pub const fn raft(&self) -> &Raft<TypeConfig> {
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
        Arc::new(OwnershipRpcHandler {
            raft: self.raft.clone(),
        })
    }
}

struct OwnershipRpcHandler {
    raft: Raft<TypeConfig>,
}

#[async_trait::async_trait]
impl RaftRpcHandler for OwnershipRpcHandler {
    async fn handle(&self, rpc: RaftRpc, body: Bytes) -> Result<Vec<u8>, RaftRpcRejection> {
        match rpc {
            RaftRpc::AppendEntries => {
                let request: AppendEntriesRequest<TypeConfig> = decode(&body)?;
                Ok(encode(&self.raft.append_entries(request).await))
            }
            RaftRpc::Vote => {
                let request: VoteRequest<NodeId> = decode(&body)?;
                Ok(encode(&self.raft.vote(request).await))
            }
            RaftRpc::InstallSnapshot => {
                let request: InstallSnapshotRequest<TypeConfig> = decode(&body)?;
                Ok(encode(&self.raft.install_snapshot(request).await))
            }
        }
    }
}

fn decode<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T, RaftRpcRejection> {
    serde_json::from_slice(body).map_err(|_| RaftRpcRejection::Malformed)
}

fn encode<T: Serialize>(response: &T) -> Vec<u8> {
    serde_json::to_vec(response).expect("a raft rpc response serializes to JSON")
}
