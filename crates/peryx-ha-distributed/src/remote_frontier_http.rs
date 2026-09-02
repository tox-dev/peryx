//! Authenticated HTTP transport for authority-scoped metadata frontiers. A missing frontier maps to
//! `404`, which the client treats as no report and polls again. A node that cannot read its own
//! position answers `500` instead, so a storage fault is retried as a fault rather than counted as a
//! definite absence. Epoch remains part of the reply so the durability fold can reject fenced reports.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
pub use peryx_ha::{FrontierReadError, FrontierReply, MetadataFrontierProvider};
use reqwest::Url;

use crate::client_transport::{
    HttpClientConfigError, HttpClientTransport, ReplicationStatus, classify_status, replication_error,
    require_replication_success,
};
use crate::http::{authorized, unauthorized};
use crate::peer::TransportError;
use crate::remote_durability::RemoteAck;
use crate::remote_frontier::RemoteFrontierSource;

const FRONTIER_PATH: &str = "+replication/v1/frontier/";
const FRONTIER_ROUTE: &str = "/+replication/v1/frontier/{authority}";
/// Reject replies above this bound before materializing them.
const MAX_FRONTIER_BYTES: u64 = 4096;

#[derive(Debug, thiserror::Error)]
pub enum HttpRemoteFrontierError {
    #[error("peer replication token must not be empty")]
    EmptyToken,
    #[error("remote datacenter identity must not be empty")]
    EmptyDatacenter,
    #[error("invalid peer URL {0:?}")]
    InvalidBase(String),
}

#[derive(Clone)]
pub struct HttpRemoteFrontierSource {
    http: HttpClientTransport,
    frontier_url: Url,
    datacenter: String,
}

impl fmt::Debug for HttpRemoteFrontierSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpRemoteFrontierSource")
            .field("frontier_url", &self.frontier_url)
            .field("datacenter", &self.datacenter)
            .field("http", &self.http)
            .finish_non_exhaustive()
    }
}

impl HttpRemoteFrontierSource {
    /// # Errors
    /// Returns [`HttpRemoteFrontierError`] for an empty token or datacenter, or an unusable HTTP(S) base
    /// URL.
    ///
    /// # Panics
    /// Panics if the HTTP client cannot be built.
    pub fn new(
        base: &str,
        datacenter: impl Into<String>,
        token: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, HttpRemoteFrontierError> {
        let http = HttpClientTransport::new(base, token, timeout).map_err(map_config_error)?;
        let datacenter = datacenter.into();
        if datacenter.is_empty() {
            return Err(HttpRemoteFrontierError::EmptyDatacenter);
        }
        let frontier_url = http.endpoint(FRONTIER_PATH);
        Ok(Self {
            http,
            frontier_url,
            datacenter,
        })
    }
}

#[async_trait::async_trait]
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
        let response = self.http.send(self.http.get(url)).await.map_err(replication_error)?;
        if classify_status(response.status()) == ReplicationStatus::NotFound {
            return Ok(None);
        }
        require_replication_success(response.status(), response.headers())?;
        let body = self.http.read_small_body(response, MAX_FRONTIER_BYTES).await?;
        let reply: FrontierReply = serde_json::from_slice(&body).map_err(|_| TransportError::Malformed)?;
        Ok(Some(RemoteAck {
            datacenter: self.datacenter.clone(),
            epoch: reply.epoch,
            applied_frontier: reply.applied_frontier,
        }))
    }
}

fn map_config_error(error: HttpClientConfigError) -> HttpRemoteFrontierError {
    match error {
        HttpClientConfigError::EmptyToken => HttpRemoteFrontierError::EmptyToken,
        HttpClientConfigError::InvalidBase(base) => HttpRemoteFrontierError::InvalidBase(base),
    }
}

#[derive(Clone)]
struct FrontierHttpState {
    token: String,
    provider: Arc<dyn MetadataFrontierProvider>,
}

/// # Errors
/// Returns [`HttpRemoteFrontierError::EmptyToken`] for an empty replication credential.
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
    match state.provider.frontier(&authority).await {
        Ok(Some(reply)) => Json(reply).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
