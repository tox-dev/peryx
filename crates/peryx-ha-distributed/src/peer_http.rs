//! HTTP [`PeerTransport`]. Connection loss, timeout, and transient server errors are retryable. Client,
//! authentication, capability, bound, and framing errors are terminal. The client limits response bytes
//! before materializing the body.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::Url;

use crate::client_transport::{
    HttpClientConfigError, HttpClientTransport, replication_error, require_replication_success,
};
use crate::peer::{BatchFrame, BatchRequest, PeerTransport, TransferLimits, TransportError};
use crate::protocol::ChangePage;

const CHANGES_PATH: &str = "+replication/v1/changes";

#[derive(Debug, thiserror::Error)]
pub enum HttpPeerError {
    #[error("peer replication token must not be empty")]
    EmptyToken,
    #[error("invalid peer URL {0:?}")]
    InvalidBase(String),
}

#[derive(Clone)]
pub struct HttpPeerTransport {
    http: HttpClientTransport,
    changes_url: Url,
    limits: TransferLimits,
}

impl fmt::Debug for HttpPeerTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpPeerTransport")
            .field("changes_url", &self.changes_url)
            .field("limits", &self.limits)
            .field("http", &self.http)
            .finish_non_exhaustive()
    }
}

impl HttpPeerTransport {
    /// # Errors
    /// Returns [`HttpPeerError`] for an empty token or unusable HTTP(S) base URL.
    ///
    /// # Panics
    /// Panics if the HTTP client cannot be built.
    pub fn new(
        base: &str,
        token: impl Into<String>,
        limits: TransferLimits,
        timeout: Duration,
    ) -> Result<Self, HttpPeerError> {
        let http = HttpClientTransport::new(base, token, timeout).map_err(map_config_error)?;
        let changes_url = http.endpoint(CHANGES_PATH);
        Ok(Self {
            http,
            changes_url,
            limits,
        })
    }
}

#[async_trait]
impl PeerTransport for HttpPeerTransport {
    async fn fetch_batch(&self, request: BatchRequest) -> Result<BatchFrame, TransportError> {
        if request.max_operations > self.limits.max_operations {
            return Err(TransportError::TooManyOperations {
                limit: self.limits.max_operations.get(),
                actual: request.max_operations.get(),
            });
        }
        let mut url = self.changes_url.clone();
        url.query_pairs_mut()
            .append_pair("after", &request.after.to_string())
            .append_pair("limit", &request.max_operations.get().to_string());
        let response = self.http.send(self.http.get(url)).await.map_err(replication_error)?;
        require_replication_success(response.status())?;
        let cap = self.limits.max_encoded_bytes.get();
        let body = self.http.read_replication_body(response, cap, true).await?;
        let page: ChangePage = serde_json::from_slice(&body).map_err(|_| TransportError::Malformed)?;
        Ok(BatchFrame::new(page))
    }
}

fn map_config_error(error: HttpClientConfigError) -> HttpPeerError {
    match error {
        HttpClientConfigError::EmptyToken => HttpPeerError::EmptyToken,
        HttpClientConfigError::InvalidBase(base) => HttpPeerError::InvalidBase(base),
    }
}
