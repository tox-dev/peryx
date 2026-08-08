//! The HTTP protocol behind the remote-frontier transport: the endpoint a datacenter serves to report
//! how far it has durably applied an authority's metadata, and the client a write drives against one
//! remote datacenter.
//!
//! The endpoint answers one authority-scoped question - how far has this node durably applied, and under
//! which authority epoch? The applied frontier is the node's committed metadata serial; the epoch is the
//! authority's own committed epoch, so a remote that has fenced past the write's epoch is excluded by the
//! [pure fold](crate::assess_remote_metadata_durability) rather than counted. A node that cannot report
//! answers `404`, which the client reads as "not applying yet" rather than a failure, so a write keeps
//! polling a remote that is still catching up. The endpoint is bearer-authenticated on the same
//! replication credential as the change and receipt endpoints.
//!
//! [`HttpRemoteFrontierSource`] is the production [`RemoteFrontierSource`]; the in-process
//! [`LoopbackRemoteFrontierSource`](crate::remote_frontier::LoopbackRemoteFrontierSource) is its test
//! counterpart. HTTP failures map onto the shared [`TransportError`] taxonomy so a gather treats a
//! frontier loss the way it treats any remote loss.
//!
//! [`RemoteFrontierSource`]: crate::remote_frontier::RemoteFrontierSource

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};

use crate::http::{authorized, unauthorized};
use crate::peer::TransportError;
use crate::remote_durability::RemoteAck;
use crate::remote_frontier::RemoteFrontierSource;

const FRONTIER_PATH: &str = "+replication/v1/frontier/";
const FRONTIER_ROUTE: &str = "/+replication/v1/frontier/{authority}";
const USER_AGENT: &str = concat!("peryx-ha-distributed/", env!("CARGO_PKG_VERSION"));
/// A frontier reply is a single small JSON object; a remote that streams past this is rejected as
/// malformed rather than read unbounded.
const MAX_FRONTIER_BYTES: u64 = 4096;

/// A remote datacenter's report of how far it has durably applied an authority's metadata: the authority
/// epoch it applied under and the highest metadata serial it has made durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontierReply {
    /// The authority epoch this node has committed for the queried authority.
    pub epoch: u64,
    /// The highest metadata serial this node has durably applied.
    pub applied_frontier: u64,
}

/// What a node reports for an authority's frontier, so the server endpoint reads durability without
/// depending on the driver's application state.
///
/// The binary implements this over its ownership authority and metadata store; the replication crate only
/// serves the answer.
#[async_trait]
pub trait MetadataFrontierProvider: Send + Sync {
    /// This node's frontier for `authority`, or `None` when it cannot report one - an absent group or an
    /// unreadable serial, which the write treats as a remote not applying yet rather than a failure.
    async fn frontier(&self, authority: &str) -> Option<FrontierReply>;
}

/// Building an [`HttpRemoteFrontierSource`] failed before any request left the process.
#[derive(Debug, thiserror::Error)]
pub enum HttpRemoteFrontierError {
    #[error("peer replication token must not be empty")]
    EmptyToken,
    #[error("remote datacenter identity must not be empty")]
    EmptyDatacenter,
    #[error("invalid peer URL {0:?}")]
    InvalidBase(String),
}

/// A bearer-authenticated HTTP [`RemoteFrontierSource`] that asks one remote datacenter how far it has
/// durably applied an authority's metadata.
#[derive(Clone)]
pub struct HttpRemoteFrontierSource {
    http: Client,
    frontier_url: Url,
    datacenter: String,
    token: String,
}

impl fmt::Debug for HttpRemoteFrontierSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpRemoteFrontierSource")
            .field("frontier_url", &self.frontier_url)
            .field("datacenter", &self.datacenter)
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

fn endpoint_url(base: &Url, path: &str) -> Url {
    let mut url = base.clone();
    url.set_path(&format!("{}{path}", base.path()));
    url
}

impl HttpRemoteFrontierSource {
    /// Build a source querying the remote `datacenter` at its server URL, bounding each request with
    /// `timeout`.
    ///
    /// # Errors
    /// Returns [`HttpRemoteFrontierError`] for an empty token or datacenter, or a URL that is not a usable
    /// HTTP(S) base.
    ///
    /// # Panics
    /// Panics if the HTTP client cannot be built, which a static user agent and a duration timeout over
    /// the guaranteed `rustls` provider never provoke.
    pub fn new(
        base: &str,
        datacenter: impl Into<String>,
        token: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, HttpRemoteFrontierError> {
        let token = token.into();
        if token.is_empty() {
            return Err(HttpRemoteFrontierError::EmptyToken);
        }
        let datacenter = datacenter.into();
        if datacenter.is_empty() {
            return Err(HttpRemoteFrontierError::EmptyDatacenter);
        }
        let Ok(mut base_url) = Url::parse(base) else {
            return Err(HttpRemoteFrontierError::InvalidBase(base.to_owned()));
        };
        if !matches!(base_url.scheme(), "http" | "https") || base_url.cannot_be_a_base() {
            return Err(HttpRemoteFrontierError::InvalidBase(base.to_owned()));
        }
        if !base_url.path().ends_with('/') {
            base_url.set_path(&format!("{}/", base_url.path()));
        }
        base_url.set_query(None);
        base_url.set_fragment(None);
        let frontier_url = endpoint_url(&base_url, FRONTIER_PATH);
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(timeout)
            .build()
            .expect("a reqwest client with a static user agent and a duration timeout always builds");
        Ok(Self {
            http,
            frontier_url,
            datacenter,
            token,
        })
    }
}

/// Map a transport-layer failure to its retryable [`TransportError`]: a deadline becomes a
/// [`Timeout`](TransportError::Timeout), any other connection loss a
/// [`Disconnected`](TransportError::Disconnected).
fn classify_loss(error: &reqwest::Error) -> TransportError {
    if error.is_timeout() {
        TransportError::Timeout
    } else {
        TransportError::Disconnected
    }
}

#[async_trait]
impl RemoteFrontierSource for HttpRemoteFrontierSource {
    fn datacenter(&self) -> &str {
        &self.datacenter
    }

    async fn fetch_frontier(&self, authority: &str) -> Result<Option<RemoteAck>, TransportError> {
        let mut url = self.frontier_url.clone();
        url.path_segments_mut()
            .expect("the frontier base is a valid base URL")
            .pop_if_empty()
            .push(authority);
        let mut response = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|error| classify_loss(&error))?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED {
            return Err(TransportError::Unauthenticated);
        }
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        // A transient 5xx (an overloaded or restarting remote) is retryable, so the gather re-polls it
        // while the deadline is live; a permanent `501` capability gap stays terminal alongside the 4xx.
        if status.is_server_error() && status != StatusCode::NOT_IMPLEMENTED {
            return Err(TransportError::ServerError {
                status: status.as_u16(),
            });
        }
        if !status.is_success() {
            return Err(TransportError::BadStatus {
                status: status.as_u16(),
            });
        }
        // Read the small reply under a byte cap without trusting an advertised length, so a remote that
        // streams an unbounded body is rejected before the buffer grows past the cap.
        let mut body: Vec<u8> = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| classify_loss(&error))? {
            if body.len() as u64 + chunk.len() as u64 > MAX_FRONTIER_BYTES {
                return Err(TransportError::Malformed);
            }
            body.extend_from_slice(&chunk);
        }
        let reply: FrontierReply = serde_json::from_slice(&body).map_err(|_| TransportError::Malformed)?;
        Ok(Some(RemoteAck {
            datacenter: self.datacenter.clone(),
            epoch: reply.epoch,
            applied_frontier: reply.applied_frontier,
        }))
    }
}

/// The frontier provider a remote-frontier endpoint reads durability from, behind its bearer credential.
#[derive(Clone)]
struct FrontierHttpState {
    token: String,
    provider: Arc<dyn MetadataFrontierProvider>,
}

/// Build the authenticated remote-frontier endpoint over `provider`, answering same-authority frontier
/// queries from other datacenters behind `token`.
///
/// # Errors
/// Returns [`HttpRemoteFrontierError::EmptyToken`] when the replication credential is empty.
pub fn frontier_router(
    token: impl Into<String>,
    provider: Arc<dyn MetadataFrontierProvider>,
) -> Result<Router, HttpRemoteFrontierError> {
    let token = token.into();
    if token.is_empty() {
        return Err(HttpRemoteFrontierError::EmptyToken);
    }
    Ok(Router::new()
        .route(FRONTIER_ROUTE, get(serve_frontier))
        .with_state(FrontierHttpState { token, provider }))
}

async fn serve_frontier(
    State(state): State<FrontierHttpState>,
    headers: HeaderMap,
    Path(authority): Path<String>,
) -> Response {
    if !authorized(&headers, &state.token) {
        return unauthorized();
    }
    state.provider.frontier(&authority).await.map_or_else(
        || StatusCode::NOT_FOUND.into_response(),
        |reply| Json(reply).into_response(),
    )
}
