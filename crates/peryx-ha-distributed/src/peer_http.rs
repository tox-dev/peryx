//! HTTP [`PeerTransport`]. Connection loss, timeout, and transient server errors are retryable. Client,
//! authentication, capability, bound, and framing errors are terminal. The client limits response bytes
//! before materializing the body.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{StatusCode, Url};

use crate::http::{CHECKPOINT_CURSOR_HEADER, MAX_CHECKPOINT_CHUNK_BYTES};
use peryx_storage::meta::CheckpointManifest;

use crate::client_transport::{
    HttpClientConfigError, HttpClientTransport, replication_error, require_replication_success,
};
use crate::peer::{
    BatchFrame, BatchRequest, CheckpointWindow, PeerTransport, TransferLimits, TransportError, validate_batch_size,
};
use crate::protocol::ChangePage;

const CHANGES_PATH: &str = "+replication/v1/changes";
const CHECKPOINT_PATH: &str = "+replication/v1/checkpoint";
const CHECKPOINT_CHUNK_PATH: &str = "+replication/v1/checkpoint/chunk";

/// Caps a manifest reply, which holds counts, a serial and a digest and nothing that scales.
const MAX_CHECKPOINT_MANIFEST_BYTES: u64 = 8 * 1024;

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
    checkpoint_url: Url,
    checkpoint_chunk_url: Url,
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
        let checkpoint_url = http.endpoint(CHECKPOINT_PATH);
        let checkpoint_chunk_url = http.endpoint(CHECKPOINT_CHUNK_PATH);
        Ok(Self {
            http,
            changes_url,
            checkpoint_url,
            checkpoint_chunk_url,
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
        let cap = self.limits.max_encoded_bytes.get();
        // The writer refuses a page it cannot build under its byte bound, and the record it stopped
        // at is the one after the cursor.
        if response.status() == StatusCode::PAYLOAD_TOO_LARGE {
            return Err(TransportError::RecordTooLarge {
                serial: request.after + 1,
                limit: cap,
            });
        }
        require_replication_success(response.status(), response.headers())?;
        let body = self.http.read_replication_body(response, cap, true).await?;
        let page: ChangePage = serde_json::from_slice(&body).map_err(|_| TransportError::Malformed)?;
        validate_batch_size(request.max_operations, &page)?;
        Ok(BatchFrame::from_encoded(page, body.len() as u64))
    }

    async fn checkpoint_manifest(&self) -> Result<CheckpointManifest, TransportError> {
        let url = self.checkpoint_url.clone();
        let response = self.http.send(self.http.get(url)).await.map_err(replication_error)?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(TransportError::CheckpointUnavailable);
        }
        require_replication_success(response.status(), response.headers())?;
        let body = self
            .http
            .read_replication_body(response, MAX_CHECKPOINT_MANIFEST_BYTES, true)
            .await?;
        serde_json::from_slice(&body).map_err(|_| TransportError::Malformed)
    }

    async fn checkpoint_chunk(&self, cursor: &str) -> Result<CheckpointWindow, TransportError> {
        let mut url = self.checkpoint_chunk_url.clone();
        url.query_pairs_mut().append_pair("cursor", cursor);
        // A writer with nothing published answers an empty window rather than a miss, so this reads the
        // same for a reader that arrived without a manifest as for one that ran past the end.
        let response = self.http.send(self.http.get(url)).await.map_err(replication_error)?;
        require_replication_success(response.status(), response.headers())?;
        let next = response
            .headers()
            .get(CHECKPOINT_CURSOR_HEADER)
            .and_then(|value| value.to_str().ok())
            .ok_or(TransportError::Malformed)?
            .to_owned();
        // The window is bytes rather than a framed page, so the change-page cap is the wrong bound; a
        // window is capped by what the writer serves in one.
        let bytes = self
            .http
            .read_replication_body(response, MAX_CHECKPOINT_CHUNK_BYTES as u64 * 2, true)
            .await?;
        Ok(CheckpointWindow { bytes, next })
    }
}

fn map_config_error(error: HttpClientConfigError) -> HttpPeerError {
    match error {
        HttpClientConfigError::EmptyToken => HttpPeerError::EmptyToken,
        HttpClientConfigError::InvalidBase(base) => HttpPeerError::InvalidBase(base),
    }
}
