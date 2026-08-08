//! The `OpenRaft` `RaftNetwork` adapter for the ownership consensus group: sends append-entries, vote,
//! and install-snapshot RPCs to peer voters.
//!
//! The network lane fills this module with the `RaftNetwork<TypeConfig>` implementation and its client
//! and factory, dialing each peer at its [`PeryxNode`](crate::raft::PeryxNode) address. The foundation
//! declares the module so the `raft` tree is owned in one place and the adapter only adds its
//! implementation here.

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
use reqwest::{Client, Url};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::http::{authorized, unauthorized};

use openraft::error::{InstallSnapshotError, NetworkError, RPCError, RaftError, Unreachable};
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse, VoteRequest,
    VoteResponse,
};

use crate::raft::{PeryxNode, TypeConfig};

const APPEND_ENTRIES_PATH: &str = "+replication/v1/raft/append-entries";
const VOTE_PATH: &str = "+replication/v1/raft/vote";
const INSTALL_SNAPSHOT_PATH: &str = "+replication/v1/raft/install-snapshot";
const USER_AGENT: &str = concat!("peryx-ha-distributed/", env!("CARGO_PKG_VERSION"));

/// A conservative cap on a single RPC response body: 64 MiB, comfortably above an `OpenRaft` snapshot
/// chunk while still bounding a hostile or broken peer's reply.
pub const DEFAULT_MAX_RPC_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;

/// The largest inbound RPC body the endpoints accept, matching the response cap.
///
/// axum defaults a body to 2 MiB; an append-entries batch or a snapshot chunk past that would be
/// rejected with `413`, which the sending client maps to a terminal error and never retries, stalling
/// replication. Sizing the inbound limit to the bodies a peer actually sends keeps a large batch flowing.
pub const DEFAULT_MAX_RPC_REQUEST_BYTES: usize = 64 * 1024 * 1024;

/// The three RPCs a Raft node sends a peer. Each maps to one authenticated endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RaftRpc {
    AppendEntries,
    Vote,
    InstallSnapshot,
}

impl RaftRpc {
    const fn path(self) -> &'static str {
        match self {
            Self::AppendEntries => APPEND_ENTRIES_PATH,
            Self::Vote => VOTE_PATH,
            Self::InstallSnapshot => INSTALL_SNAPSHOT_PATH,
        }
    }

    /// A stable label for logs and metrics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AppendEntries => "append_entries",
            Self::Vote => "vote",
            Self::InstallSnapshot => "install_snapshot",
        }
    }
}

/// Building a [`RaftRpcClient`] or a [`raft_rpc_router`] failed before any request moved.
#[derive(Debug, thiserror::Error)]
pub enum RaftRpcConfigError {
    #[error("raft replication token must not be empty")]
    EmptyToken,
    #[error("invalid peer URL {0:?}")]
    InvalidBase(String),
}

/// A Raft RPC failed. Split so a [`RaftNetwork`](https://docs.rs/openraft) implementation can report an
/// unreachable peer (retry with backoff) apart from a terminal protocol failure.
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
    /// Whether the peer looks unreachable, so the caller should retry after a backoff rather than fail
    /// the RPC to the Raft core. A remote status, an auth reject, an oversized reply, and a malformed
    /// body are terminal instead.
    #[must_use]
    pub const fn is_unreachable(&self) -> bool {
        matches!(self, Self::Unreachable | Self::Timeout)
    }
}

/// Map a transport-layer failure to its [`RaftRpcError`]: a deadline becomes a
/// [`Timeout`](RaftRpcError::Timeout), any other connection loss an
/// [`Unreachable`](RaftRpcError::Unreachable).
fn classify_loss(error: &reqwest::Error) -> RaftRpcError {
    if error.is_timeout() {
        RaftRpcError::Timeout
    } else {
        RaftRpcError::Unreachable
    }
}

fn endpoint_url(base: &Url, path: &str) -> Url {
    let mut url = base.clone();
    url.set_path(&format!("{}{path}", base.path()));
    url
}

/// A bearer-authenticated HTTP client for one peer's Raft RPC endpoints. An `OpenRaft` `RaftNetwork`
/// bound to a target node drives its three methods through [`send`](RaftRpcClient::send).
#[derive(Clone)]
pub struct RaftRpcClient {
    http: Client,
    base: Url,
    token: String,
    max_response_bytes: u64,
}

impl fmt::Debug for RaftRpcClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RaftRpcClient")
            .field("base", &self.base)
            .field("max_response_bytes", &self.max_response_bytes)
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl RaftRpcClient {
    /// Build a client rooted at the peer server URL, bounding each RPC with `timeout`.
    ///
    /// # Errors
    /// Returns [`RaftRpcConfigError`] for an empty token or a URL that is not a usable HTTP(S) base.
    ///
    /// # Panics
    /// Panics if the HTTP client cannot be built, which a static user agent and a duration timeout over
    /// the guaranteed `rustls` provider never provoke.
    pub fn new(base: &str, token: impl Into<String>, timeout: Duration) -> Result<Self, RaftRpcConfigError> {
        let token = token.into();
        if token.is_empty() {
            return Err(RaftRpcConfigError::EmptyToken);
        }
        let Ok(mut base_url) = Url::parse(base) else {
            return Err(RaftRpcConfigError::InvalidBase(base.to_owned()));
        };
        if !matches!(base_url.scheme(), "http" | "https") || base_url.cannot_be_a_base() {
            return Err(RaftRpcConfigError::InvalidBase(base.to_owned()));
        }
        if !base_url.path().ends_with('/') {
            base_url.set_path(&format!("{}/", base_url.path()));
        }
        base_url.set_query(None);
        base_url.set_fragment(None);
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(timeout)
            .build()
            .expect("a reqwest client with a static user agent and a duration timeout always builds");
        Ok(Self {
            http,
            base: base_url,
            token,
            max_response_bytes: DEFAULT_MAX_RPC_RESPONSE_BYTES,
        })
    }

    /// Cap a single RPC response body at `bytes`, overriding [`DEFAULT_MAX_RPC_RESPONSE_BYTES`].
    #[must_use]
    pub const fn with_response_cap(mut self, bytes: u64) -> Self {
        self.max_response_bytes = bytes;
        self
    }

    /// Send `request` as `rpc` and decode the peer's response.
    ///
    /// # Errors
    /// Returns a [`RaftRpcError`]: unreachable or timeout for transport loss, unauthenticated for a
    /// credential reject, a remote status for any other non-success, oversized for a reply past the byte
    /// cap, and malformed for a reply that does not decode.
    ///
    /// # Panics
    /// Panics only if `request` fails to serialize to JSON, which the `OpenRaft` RPC types never do.
    pub async fn send<Req, Resp>(&self, rpc: RaftRpc, request: &Req) -> Result<Resp, RaftRpcError>
    where
        Req: Serialize + Sync,
        Resp: DeserializeOwned,
    {
        let url = endpoint_url(&self.base, rpc.path());
        let body = serde_json::to_vec(request).expect("a raft rpc request serializes");
        let mut response = self
            .http
            .post(url)
            .bearer_auth(&self.token)
            .header(header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(|error| classify_loss(&error))?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED {
            return Err(RaftRpcError::Unauthenticated);
        }
        if !status.is_success() {
            return Err(RaftRpcError::RemoteError {
                status: status.as_u16(),
            });
        }
        let mut body: Vec<u8> = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| classify_loss(&error))? {
            let total = body.len() as u64 + chunk.len() as u64;
            if total > self.max_response_bytes {
                return Err(RaftRpcError::ResponseTooLarge {
                    limit: self.max_response_bytes,
                    actual: total,
                });
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|_| RaftRpcError::Malformed)
    }
}

/// The reason a peer RPC endpoint refused a request body before the local node saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RaftRpcRejection {
    #[error("raft rpc body could not be decoded")]
    Malformed,
}

/// The server side of the transport: the local Raft node behind the peer RPC endpoint.
///
/// An `OpenRaft` integration implements this by decoding `body` as the request type `rpc` names, calling
/// the matching method on its `Raft` handle, and encoding the response. Working in bytes keeps this
/// trait independent of the `RaftTypeConfig`, so the router below carries any cluster's RPCs.
#[async_trait]
pub trait RaftRpcHandler: Send + Sync + 'static {
    /// Decode `body` as the request `rpc` names, drive the local node, and return the encoded response.
    ///
    /// # Errors
    /// Returns [`RaftRpcRejection::Malformed`] when `body` is not a valid request for `rpc`.
    async fn handle(&self, rpc: RaftRpc, body: Bytes) -> Result<Vec<u8>, RaftRpcRejection>;
}

#[derive(Clone)]
struct RaftRpcState {
    token: String,
    handler: Arc<dyn RaftRpcHandler>,
}

/// Build the authenticated peer RPC routes that receive a Raft node's inbound `AppendEntries`, Vote, and
/// `InstallSnapshot` RPCs and dispatch them to `handler`.
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
        .route(&format!("/{VOTE_PATH}"), post(dispatch_vote))
        .route(&format!("/{INSTALL_SNAPSHOT_PATH}"), post(dispatch_install_snapshot))
        .layer(DefaultBodyLimit::max(DEFAULT_MAX_RPC_REQUEST_BYTES))
        .with_state(state))
}

async fn dispatch_append_entries(State(state): State<RaftRpcState>, headers: HeaderMap, body: Bytes) -> Response {
    dispatch(&state, RaftRpc::AppendEntries, &headers, body).await
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

/// Map a transport failure to openraft's [`RPCError`].
///
/// The retryable arms become `Unreachable`, which openraft retries under backoff; every other arm
/// becomes a `Network` error. A remote voter's own Raft error rides back inside the response body and
/// also maps to `Network`, so no remote error is typed.
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

/// A peer voter's network client, one per target.
///
/// Holds the built [`RaftRpcClient`], or the error from a voter addr that does not form a valid base, so
/// a bad roster entry fails that peer's RPCs retryably instead of panicking the replication task.
/// [`RaftRpcClient::send`] takes `&self`, so the trait's `&mut self` needs no extra state.
pub struct PeerRaftNetwork {
    client: Result<RaftRpcClient, RaftRpcConfigError>,
}

impl PeerRaftNetwork {
    /// Send `request` as `rpc` and lift the reply into openraft's result. A client that never built (an
    /// unparseable voter addr) maps to [`Unreachable`](RPCError::Unreachable), and so does a transport
    /// loss by [`unreachable_or_network`], so openraft retries under backoff rather than the task
    /// panicking. A remote voter encodes `Result<Response, RaftError>`, so its own Raft error rides back
    /// in the body and maps to a `Network` error rather than a typed remote one.
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
        wire.map_err(|error| RPCError::Network(NetworkError::new(&error)))
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

/// Builds one [`PeerRaftNetwork`] per target voter, dialing the address its [`PeryxNode`] carries.
pub struct PeerRaftNetworkFactory {
    token: String,
    timeout: Duration,
}

impl PeerRaftNetworkFactory {
    /// A factory that dials each peer with `token` and bounds every RPC by `timeout`.
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

    async fn new_client(&mut self, _target: NodeId, node: &PeryxNode) -> Self::Network {
        // openraft builds a peer client lazily inside the replication task and `new_client` cannot fail,
        // so a voter addr that does not form a valid base must not panic here. Keep the build error and
        // surface it as an Unreachable RPC, so a bad roster entry fails that peer retryably rather than
        // aborting the node.
        let base = format!("http://{}/", node.addr);
        PeerRaftNetwork {
            client: RaftRpcClient::new(&base, self.token.clone(), self.timeout),
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/raft/network_tests.rs"]
mod network_tests;
