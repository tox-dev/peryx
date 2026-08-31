use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use axum::routing::post;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::client_transport::{
    HttpClientConfigError, HttpClientError, HttpClientTransport, ReplicationStatus, classify_status,
};
use crate::http::{authorized, unauthorized};

use openraft::error::{InstallSnapshotError, NetworkError, RPCError, RaftError, RemoteError, Unreachable};
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse, VoteRequest,
    VoteResponse,
};

use crate::raft::{PeryxNode, TypeConfig};

const APPEND_ENTRIES_PATH: &str = "+replication/v1/raft/append-entries";
const CLIENT_WRITE_PATH: &str = "+replication/v1/raft/client-write";
const VOTE_PATH: &str = "+replication/v1/raft/vote";
const INSTALL_SNAPSHOT_PATH: &str = "+replication/v1/raft/install-snapshot";

/// Caps peer responses at 64 MiB, above an `OpenRaft` snapshot chunk while bounding memory consumed by
/// broken or hostile peers.
pub const DEFAULT_MAX_RPC_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;

/// Matches the response cap. axum's 2 MiB default can reject append batches or snapshot chunks with
/// `413`; the transport treats that status as terminal, which stalls replication.
pub const DEFAULT_MAX_RPC_REQUEST_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaftRpc {
    AppendEntries,
    ClientWrite,
    Vote,
    InstallSnapshot,
}

impl RaftRpc {
    const fn path(self) -> &'static str {
        match self {
            Self::AppendEntries => APPEND_ENTRIES_PATH,
            Self::ClientWrite => CLIENT_WRITE_PATH,
            Self::Vote => VOTE_PATH,
            Self::InstallSnapshot => INSTALL_SNAPSHOT_PATH,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AppendEntries => "append_entries",
            Self::ClientWrite => "client_write",
            Self::Vote => "vote",
            Self::InstallSnapshot => "install_snapshot",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RaftRpcConfigError {
    #[error("raft replication token must not be empty")]
    EmptyToken,
    #[error("invalid peer URL {0:?}")]
    InvalidBase(String),
}

/// `OpenRaft` retries transport loss and timeouts; it does not retry protocol failures.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RaftRpcError {
    #[error("peer unreachable before the rpc completed")]
    Unreachable,
    #[error("peer did not answer within the rpc deadline")]
    Timeout,
    #[error("peer rejected the replication credential")]
    Unauthenticated,
    #[error("peer raft endpoint returned status {status}")]
    RemoteError { status: u16 },
    #[error("peer raft reply is {actual} bytes; the transport caps a reply at {limit}")]
    ResponseTooLarge { limit: u64, actual: u64 },
    #[error("peer raft reply could not be decoded")]
    Malformed,
}

impl RaftRpcError {
    #[must_use]
    pub const fn is_unreachable(&self) -> bool {
        matches!(self, Self::Unreachable | Self::Timeout)
    }
}

#[derive(Clone)]
pub struct RaftRpcClient {
    http: HttpClientTransport,
    max_response_bytes: u64,
}

impl fmt::Debug for RaftRpcClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RaftRpcClient")
            .field("max_response_bytes", &self.max_response_bytes)
            .field("http", &self.http)
            .finish_non_exhaustive()
    }
}

impl RaftRpcClient {
    /// # Errors
    /// Returns [`RaftRpcConfigError`] for an empty token or a URL that is not a usable HTTP(S) base.
    ///
    /// # Panics
    /// Panics if HTTP client construction fails.
    pub fn new(base: &str, token: impl Into<String>, timeout: Duration) -> Result<Self, RaftRpcConfigError> {
        Ok(Self {
            http: HttpClientTransport::new(base, token, timeout).map_err(map_config_error)?,
            max_response_bytes: DEFAULT_MAX_RPC_RESPONSE_BYTES,
        })
    }

    #[must_use]
    pub const fn with_response_cap(mut self, bytes: u64) -> Self {
        self.max_response_bytes = bytes;
        self
    }

    /// # Errors
    /// Returns [`RaftRpcError::Unreachable`] or [`RaftRpcError::Timeout`] for transport loss and a
    /// terminal variant for authentication, status, size, or decoding failures.
    ///
    /// # Panics
    /// Panics if `request` serialization fails; `OpenRaft` RPC types serialize to JSON.
    pub async fn send<Req, Resp>(&self, rpc: RaftRpc, request: &Req) -> Result<Resp, RaftRpcError>
    where
        Req: Serialize + Sync,
        Resp: DeserializeOwned,
    {
        let body = serde_json::to_vec(request).expect("a raft rpc request serializes");
        let response = self
            .http
            .send(
                self.http
                    .post(self.http.endpoint(rpc.path()))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(body),
            )
            .await
            .map_err(map_client_error)?;
        require_rpc_success(response.status())?;
        let body = self
            .http
            .read_bounded(response, self.max_response_bytes, false)
            .await
            .map_err(map_client_error)?;
        serde_json::from_slice(&body).map_err(|_| RaftRpcError::Malformed)
    }
}

const fn require_rpc_success(status: StatusCode) -> Result<(), RaftRpcError> {
    match classify_status(status) {
        ReplicationStatus::Success => Ok(()),
        ReplicationStatus::Unauthenticated => Err(RaftRpcError::Unauthenticated),
        ReplicationStatus::NotFound | ReplicationStatus::ServerError(_) | ReplicationStatus::BadStatus(_) => {
            Err(RaftRpcError::RemoteError {
                status: status.as_u16(),
            })
        }
    }
}

fn map_config_error(error: HttpClientConfigError) -> RaftRpcConfigError {
    match error {
        HttpClientConfigError::EmptyToken => RaftRpcConfigError::EmptyToken,
        HttpClientConfigError::InvalidBase(base) => RaftRpcConfigError::InvalidBase(base),
    }
}

const fn map_client_error(error: HttpClientError) -> RaftRpcError {
    match error {
        HttpClientError::Timeout => RaftRpcError::Timeout,
        HttpClientError::Disconnected => RaftRpcError::Unreachable,
        HttpClientError::BodyTooLarge { limit, actual } => RaftRpcError::ResponseTooLarge { limit, actual },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RaftRpcRejection {
    #[error("raft rpc body could not be decoded")]
    Malformed,
}

/// Uses byte payloads to avoid binding the router to `RaftTypeConfig`.
#[async_trait]
pub trait RaftRpcHandler: Send + Sync + 'static {
    /// # Errors
    /// Returns [`RaftRpcRejection::Malformed`] when `body` is not a valid request for `rpc`.
    async fn handle(&self, rpc: RaftRpc, body: Bytes) -> Result<Vec<u8>, RaftRpcRejection>;
}

#[derive(Clone)]
struct RaftRpcState {
    token: String,
    handler: Arc<dyn RaftRpcHandler>,
}

/// Builds bearer-authenticated endpoints for all three peer RPCs.
///
/// # Errors
/// Returns [`RaftRpcConfigError::EmptyToken`] when the bearer token is empty.
pub fn raft_rpc_router(
    token: impl Into<String>,
    handler: Arc<dyn RaftRpcHandler>,
) -> Result<Router, RaftRpcConfigError> {
    let token = token.into();
    if token.is_empty() {
        return Err(RaftRpcConfigError::EmptyToken);
    }
    let state = RaftRpcState { token, handler };
    Ok(Router::new()
        .route(&format!("/{APPEND_ENTRIES_PATH}"), post(dispatch_append_entries))
        .route(&format!("/{CLIENT_WRITE_PATH}"), post(dispatch_client_write))
        .route(&format!("/{VOTE_PATH}"), post(dispatch_vote))
        .route(&format!("/{INSTALL_SNAPSHOT_PATH}"), post(dispatch_install_snapshot))
        .layer(DefaultBodyLimit::max(DEFAULT_MAX_RPC_REQUEST_BYTES))
        .with_state(state))
}

async fn dispatch_append_entries(State(state): State<RaftRpcState>, headers: HeaderMap, body: Bytes) -> Response {
    dispatch(&state, RaftRpc::AppendEntries, &headers, body).await
}

async fn dispatch_client_write(State(state): State<RaftRpcState>, headers: HeaderMap, body: Bytes) -> Response {
    dispatch(&state, RaftRpc::ClientWrite, &headers, body).await
}

async fn dispatch_vote(State(state): State<RaftRpcState>, headers: HeaderMap, body: Bytes) -> Response {
    dispatch(&state, RaftRpc::Vote, &headers, body).await
}

async fn dispatch_install_snapshot(State(state): State<RaftRpcState>, headers: HeaderMap, body: Bytes) -> Response {
    dispatch(&state, RaftRpc::InstallSnapshot, &headers, body).await
}

async fn dispatch(state: &RaftRpcState, rpc: RaftRpc, headers: &HeaderMap, body: Bytes) -> Response {
    if !authorized(headers, &state.token) {
        return unauthorized();
    }
    match state.handler.handle(rpc, body).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, "application/json")], bytes).into_response(),
        Err(RaftRpcRejection::Malformed) => (StatusCode::BAD_REQUEST, "malformed raft rpc").into_response(),
    }
}

type NodeId = u64;

/// Maps transport loss and timeouts to retryable `Unreachable` errors; protocol failures become
/// `Network` errors.
fn unreachable_or_network<E>(error: &RaftRpcError) -> RPCError<NodeId, PeryxNode, E>
where
    E: std::error::Error,
{
    if error.is_unreachable() {
        RPCError::Unreachable(Unreachable::new(error))
    } else {
        RPCError::Network(NetworkError::new(error))
    }
}

/// Retains peer identity so `OpenRaft` can distinguish peer rejections from transport failures.
pub struct PeerRaftNetwork {
    target: NodeId,
    target_node: PeryxNode,
    client: Result<RaftRpcClient, RaftRpcConfigError>,
}

impl PeerRaftNetwork {
    async fn call<Req, Resp, E>(
        &self,
        rpc: RaftRpc,
        request: &Req,
    ) -> Result<Resp, RPCError<NodeId, PeryxNode, RaftError<NodeId, E>>>
    where
        Req: Serialize + Sync,
        Resp: DeserializeOwned,
        E: std::error::Error + 'static,
        RaftError<NodeId, E>: DeserializeOwned,
    {
        let client = self
            .client
            .as_ref()
            .map_err(|error| RPCError::Unreachable(Unreachable::new(error)))?;
        let wire: Result<Resp, RaftError<NodeId, E>> = client
            .send(rpc, request)
            .await
            .map_err(|error| unreachable_or_network(&error))?;
        wire.map_err(|error| {
            RPCError::RemoteError(RemoteError::new_with_node(self.target, self.target_node.clone(), error))
        })
    }
}

impl RaftNetwork<TypeConfig> for PeerRaftNetwork {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<AppendEntriesResponse<NodeId>, RPCError<NodeId, PeryxNode, RaftError<NodeId>>> {
        self.call(RaftRpc::AppendEntries, &rpc).await
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<NodeId>,
        _option: RPCOption,
    ) -> Result<VoteResponse<NodeId>, RPCError<NodeId, PeryxNode, RaftError<NodeId>>> {
        self.call(RaftRpc::Vote, &rpc).await
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<TypeConfig>,
        _option: RPCOption,
    ) -> Result<InstallSnapshotResponse<NodeId>, RPCError<NodeId, PeryxNode, RaftError<NodeId, InstallSnapshotError>>>
    {
        self.call(RaftRpc::InstallSnapshot, &rpc).await
    }
}

pub struct PeerRaftNetworkFactory {
    token: String,
    timeout: Duration,
}

impl PeerRaftNetworkFactory {
    #[must_use]
    pub fn new(token: impl Into<String>, timeout: Duration) -> Self {
        Self {
            token: token.into(),
            timeout,
        }
    }
}

impl RaftNetworkFactory<TypeConfig> for PeerRaftNetworkFactory {
    type Network = PeerRaftNetwork;

    async fn new_client(&mut self, target: NodeId, node: &PeryxNode) -> Self::Network {
        // OpenRaft creates clients inside replication tasks, and this trait cannot return an error. Store
        // invalid-address errors so calls return `Unreachable` without panicking the task.
        PeerRaftNetwork {
            target,
            target_node: node.clone(),
            client: RaftRpcClient::new(&node.endpoint, self.token.clone(), self.timeout),
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/raft/network_tests.rs"]
mod network_tests;
