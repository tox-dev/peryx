//! A production HTTP [`PeerTransport`]: the concrete client a replica drives against a peer's
//! `serve_changes` endpoint, in contrast to the in-process [`LoopbackTransport`] the tests use.
//!
//! It reuses the [`TransportError`] taxonomy rather than restating it, mapping each HTTP failure to the
//! arm a scheduler already knows how to act on. A connection loss, a deadline, or a transient `5xx` from
//! an overloaded primary is retryable, so it feeds the reconnect backoff; an auth reject, a `4xx` client
//! error, a `501` capability gap, an oversized frame, an over-count request, or a malformed reply fails
//! closed. The transfer bounds are checked before the reply is materialized, so a fast or hostile peer
//! cannot force an unbounded read.
//!
//! [`LoopbackTransport`]: crate::peer::LoopbackTransport

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::{Client, StatusCode, Url};

use crate::peer::{BatchFrame, BatchRequest, PeerTransport, TransferLimits, TransportError};
use crate::protocol::ChangePage;

const CHANGES_PATH: &str = "+replication/v1/changes";
const USER_AGENT: &str = concat!("peryx-replication/", env!("CARGO_PKG_VERSION"));

/// Building an [`HttpPeerTransport`] failed before any request left the process.
#[derive(Debug, thiserror::Error)]
pub enum HttpPeerError {
    #[error("peer replication token must not be empty")]
    EmptyToken,
    #[error("invalid peer URL {0:?}")]
    InvalidBase(String),
    #[error("build replication HTTP client: {0}")]
    Client(#[source] reqwest::Error),
}

/// A bearer-authenticated HTTP [`PeerTransport`] that pulls one bounded batch per fetch from a peer's
/// `serve_changes` endpoint.
#[derive(Clone)]
pub struct HttpPeerTransport {
    http: Client,
    changes_url: Url,
    token: String,
    limits: TransferLimits,
}

impl fmt::Debug for HttpPeerTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpPeerTransport")
            .field("changes_url", &self.changes_url)
            .field("limits", &self.limits)
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

fn endpoint_url(base: &Url, path: &str) -> Url {
    let mut url = base.clone();
    url.set_path(&format!("{}{path}", base.path()));
    url
}

impl HttpPeerTransport {
    /// Build a transport rooted at the peer server URL, bounding each request with `timeout` and each
    /// batch with `limits`.
    ///
    /// # Errors
    /// Returns [`HttpPeerError`] for an empty token, a URL that is not a usable HTTP(S) base, or an
    /// HTTP client that fails to build.
    pub fn new(
        base: &str,
        token: impl Into<String>,
        limits: TransferLimits,
        timeout: Duration,
    ) -> Result<Self, HttpPeerError> {
        let token = token.into();
        if token.is_empty() {
            return Err(HttpPeerError::EmptyToken);
        }
        let Ok(mut base_url) = Url::parse(base) else {
            return Err(HttpPeerError::InvalidBase(base.to_owned()));
        };
        if !matches!(base_url.scheme(), "http" | "https") || base_url.cannot_be_a_base() {
            return Err(HttpPeerError::InvalidBase(base.to_owned()));
        }
        if !base_url.path().ends_with('/') {
            base_url.set_path(&format!("{}/", base_url.path()));
        }
        base_url.set_query(None);
        base_url.set_fragment(None);
        let changes_url = endpoint_url(&base_url, CHANGES_PATH);
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(timeout)
            .build()
            .map_err(HttpPeerError::Client)?;
        Ok(Self {
            http,
            changes_url,
            token,
            limits,
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
        let response = self
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
        // A transient 5xx (an overloaded or restarting primary) is worth a backoff and retry, so it
        // maps to the retryable `ServerError`. `501 Not Implemented` is a permanent capability gap, not
        // a transient one, so it stays terminal alongside the 4xx client errors.
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
        let cap = self.limits.max_encoded_bytes.get();
        if let Some(length) = response.content_length()
            && length > cap
        {
            return Err(TransportError::FrameTooLarge {
                limit: cap,
                actual: length,
            });
        }
        let bytes = response.bytes().await.map_err(|error| classify_loss(&error))?;
        let page: ChangePage = serde_json::from_slice(&bytes).map_err(|_| TransportError::Malformed)?;
        Ok(BatchFrame::new(page))
    }
}
