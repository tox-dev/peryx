//! Assembling and driving the ownership consensus group's `OpenRaft` node.
//!
//! [`RaftNode`] wires the three adapters — the log storage, the network factory, and the
//! [`OwnershipStateMachine`] — into one [`openraft::Raft`] instance, and wraps the operations the app
//! uses: bootstrap a fresh cluster, submit an [`OwnershipCommand`], and find the current leader to
//! forward a write to. The log-storage and network adapters arrive behind their traits, so this
//! assembles against [`RaftLogStorage`] and [`RaftNetworkFactory`] and takes the concrete
//! implementations by injection.
//!
//! Only `ha` mode constructs a node; `none` and `dc` build no Raft runtime at all, so nothing here runs
//! outside a configured consensus group. Membership changes stay operator-driven: the app reaches them
//! through [`raft`](RaftNode::raft), never a liveness timeout.

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

/// The `u64` voter handle `OpenRaft` keys nodes by. See [`crate::raft`] for why it is not the datacenter
/// identity.
type NodeId = u64;

/// Why a node could not be assembled.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    /// The tuning did not pass `OpenRaft`'s validation.
    #[error("invalid raft configuration: {0}")]
    Config(#[from] Box<ConfigError>),
    /// `OpenRaft` reported a fatal error building the node.
    #[error("raft node failed to start: {0}")]
    Fatal(Box<Fatal<NodeId>>),
}

/// The ownership consensus node: an assembled [`openraft::Raft`] plus the operations the app drives it
/// with.
///
/// Holds a clone of the [`OwnershipStateMachine`] so a caller can read the applied ownership state
/// behind a linearizable barrier without routing through the log.
pub struct RaftNode {
    raft: Raft<TypeConfig>,
    state_machine: OwnershipStateMachine,
}

impl RaftNode {
    /// Assemble a node over its three adapters under `config`.
    ///
    /// Maps and validates the tuning, then builds the [`openraft::Raft`] over the injected log storage,
    /// network factory, and state machine. This starts the runtime tasks but joins no cluster; call
    /// [`bootstrap`](RaftNode::bootstrap) on the seed, or add this node through an existing leader.
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

    /// Initialize a fresh cluster from `members`, the seed roster of voter ids to their nodes.
    ///
    /// Idempotent, so increment 3 can call it on every `ha` startup: it seeds a brand-new group and is a
    /// no-op once the group is formed, since `OpenRaft` rejects a re-initialize with
    /// [`InitializeError::NotAllowed`] and this maps that to success. Adding a node to a running group
    /// goes through an existing leader instead, so this is the single-node seed or initial-roster path
    /// only.
    ///
    /// # Errors
    /// The [`RaftError`] `OpenRaft` returns when the roster is inconsistent, for example a node absent
    /// from its own membership; an already-initialized node is not an error.
    pub async fn bootstrap(
        &self,
        members: BTreeMap<NodeId, PeryxNode>,
    ) -> Result<(), RaftError<NodeId, InitializeError<NodeId, PeryxNode>>> {
        match self.raft.initialize(members).await {
            Ok(()) | Err(RaftError::APIError(InitializeError::NotAllowed(_))) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Submit `command` to the log and return its applied [`OwnershipResponse`].
    ///
    /// Only the leader accepts a write; on a follower `OpenRaft` returns a forward-to-leader error that
    /// carries the leader to retry against. Resolve it with
    /// [`forward_target`](RaftNode::forward_target), which reads that leader from the error itself.
    ///
    /// # Errors
    /// The [`RaftError`] `OpenRaft` returns when this node is not the leader or the write cannot commit.
    pub async fn submit(
        &self,
        command: OwnershipCommand,
    ) -> Result<OwnershipResponse, RaftError<NodeId, ClientWriteError<NodeId, PeryxNode>>> {
        let response = self.raft.client_write(command).await?;
        Ok(response.data)
    }

    /// The node to forward a write to after [`submit`](RaftNode::submit) rejects it, taken from the
    /// forward-to-leader error itself and falling back to [`leader`](RaftNode::leader).
    ///
    /// `OpenRaft` stamps the leader it knows onto the error at the instant it rejects the write, so that
    /// target is more current than re-deriving from local metrics, which can read `None` or a stale
    /// leader mid-election. Only when the error carries no leader does this fall back to the metrics
    /// view.
    #[must_use]
    pub fn forward_target(&self, error: &RaftError<NodeId, ClientWriteError<NodeId, PeryxNode>>) -> Option<PeryxNode> {
        if let RaftError::APIError(ClientWriteError::ForwardToLeader(forward)) = error
            && let Some(node) = &forward.leader_node
        {
            return Some(node.clone());
        }
        self.leader()
    }

    /// The node of the current leader, or `None` when no leader is known or its node is not in the
    /// membership yet. A write submitted elsewhere forwards to this address.
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

    /// The metrics watch the replica monitor observes: server state, current leader, applied log id, and
    /// membership.
    #[must_use]
    pub fn metrics(&self) -> watch::Receiver<RaftMetrics<NodeId, PeryxNode>> {
        self.raft.metrics()
    }

    /// The assembled `OpenRaft` handle, for the operations the app drives directly: membership changes
    /// (`add_learner`, `change_membership`), a linearizable-read barrier, and shutdown.
    #[must_use]
    pub const fn raft(&self) -> &Raft<TypeConfig> {
        &self.raft
    }

    /// The state machine, shared with the running node, for reading applied ownership state behind a
    /// linearizable barrier taken through [`raft`](RaftNode::raft).
    #[must_use]
    pub const fn state_machine(&self) -> &OwnershipStateMachine {
        &self.state_machine
    }

    /// The server-side RPC handler that drives this node's raft core from a peer's inbound RPCs.
    ///
    /// Mount it behind [`raft_rpc_router`](crate::raft::network::raft_rpc_router) on the peer-facing
    /// listener so remote voters can exchange `AppendEntries`, Vote, and `InstallSnapshot` with this
    /// node. Without it a node sends peer RPCs but receives none, so a group never reaches quorum.
    #[must_use]
    pub fn rpc_handler(&self) -> Arc<dyn RaftRpcHandler> {
        Arc::new(OwnershipRpcHandler {
            raft: self.raft.clone(),
        })
    }
}

/// The receive side of the peer transport: decodes a peer's RPC, drives the local raft core, and encodes
/// the reply the client decodes as `Result<Response, RaftError>`.
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
