//! The HTTP protocol behind the peer-receipt transport: the endpoint a node serves for its same-DC
//! peers, and the client a write drives against one peer.
//!
//! The endpoint answers one question about the local filesystem store - does it durably hold this
//! digest? A filesystem backend exposes only committed, content-addressed bytes, so a [`head`] hit is
//! itself the placement receipt: the peer holds the digest, and its byte length is reported for fidelity.
//! A miss is a `404`, which the client reads as "not yet" rather than a failure, so a write keeps polling
//! a peer that is still replicating the bytes. The endpoint is bearer-authenticated on the same
//! replication credential as the change and blob endpoints.
//!
//! [`HttpReceiptSource`] is the production [`ReceiptSource`]; the in-process
//! [`LoopbackReceiptSource`](crate::peer_receipt::LoopbackReceiptSource) is its test counterpart. HTTP
//! failures map onto the shared [`TransportError`] taxonomy so a gather treats a receipt loss the way it
//! treats any peer loss.
//!
//! [`head`]: peryx_storage::blob::BlobStorage::head
//! [`ReceiptSource`]: crate::peer_receipt::ReceiptSource

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use peryx_storage::blob::{BlobStorage, Digest};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};

use crate::http::{authorized, unauthorized};
use crate::peer::TransportError;
use crate::peer_receipt::{PeerReceipt, ReceiptSource};

const RECEIPTS_PATH: &str = "+replication/v1/receipts/sha256/";
const RECEIPTS_ROUTE: &str = "/+replication/v1/receipts/sha256/{digest}";
const USER_AGENT: &str = concat!("peryx-ha-distributed/", env!("CARGO_PKG_VERSION"));
/// A receipt reply is a single small JSON object; a peer that streams past this is rejected as malformed
/// rather than read unbounded.
const MAX_RECEIPT_BYTES: u64 = 4096;

/// A peer's confirmation that it durably holds the queried digest, carrying the byte length its receipt
/// verified. Presence is the proof; the `200` itself confirms the digest the client asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptReply {
    /// The durable byte length the peer holds for the digest.
    pub size: u64,
}

/// Building an [`HttpReceiptSource`] failed before any request left the process.
#[derive(Debug, thiserror::Error)]
pub enum HttpReceiptError {
    #[error("peer replication token must not be empty")]
    EmptyToken,
    #[error("peer node identity must not be empty")]
    EmptyNode,
    #[error("invalid peer URL {0:?}")]
    InvalidBase(String),
}

/// A bearer-authenticated HTTP [`ReceiptSource`] that asks one same-DC peer for its placement receipt of
/// a digest.
#[derive(Clone)]
pub struct HttpReceiptSource {
    http: Client,
    receipts_url: Url,
    node: String,
    token: String,
}

impl fmt::Debug for HttpReceiptSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpReceiptSource")
            .field("receipts_url", &self.receipts_url)
            .field("node", &self.node)
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

fn endpoint_url(base: &Url, path: &str) -> Url {
    let mut url = base.clone();
    url.set_path(&format!("{}{path}", base.path()));
    url
}

impl HttpReceiptSource {
    /// Build a source querying the peer `node` at its server URL, bounding each request with `timeout`.
    ///
    /// # Errors
    /// Returns [`HttpReceiptError`] for an empty token or node, or a URL that is not a usable HTTP(S)
    /// base.
    ///
    /// # Panics
    /// Panics if the HTTP client cannot be built, which a static user agent and a duration timeout over
    /// the guaranteed `rustls` provider never provoke.
    pub fn new(
        base: &str,
        node: impl Into<String>,
        token: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, HttpReceiptError> {
        let token = token.into();
        if token.is_empty() {
            return Err(HttpReceiptError::EmptyToken);
        }
        let node = node.into();
        if node.is_empty() {
            return Err(HttpReceiptError::EmptyNode);
        }
        let Ok(mut base_url) = Url::parse(base) else {
            return Err(HttpReceiptError::InvalidBase(base.to_owned()));
        };
        if !matches!(base_url.scheme(), "http" | "https") || base_url.cannot_be_a_base() {
            return Err(HttpReceiptError::InvalidBase(base.to_owned()));
        }
        if !base_url.path().ends_with('/') {
            base_url.set_path(&format!("{}/", base_url.path()));
        }
        base_url.set_query(None);
        base_url.set_fragment(None);
        let receipts_url = endpoint_url(&base_url, RECEIPTS_PATH);
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .timeout(timeout)
            .build()
            .expect("a reqwest client with a static user agent and a duration timeout always builds");
        Ok(Self {
            http,
            receipts_url,
            node,
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
impl ReceiptSource for HttpReceiptSource {
    fn node(&self) -> &str {
        &self.node
    }

    async fn fetch_receipt(&self, digest: &Digest) -> Result<Option<PeerReceipt>, TransportError> {
        let url = format!("{}{}", self.receipts_url, digest.as_str());
        let mut response = self
            .http
            .get(&url)
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
        // A transient 5xx (an overloaded or restarting peer) is retryable, so the gather re-polls it while
        // the deadline is live; a permanent `501` capability gap stays terminal alongside the 4xx errors.
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
        // Read the small reply under a byte cap without trusting an advertised length, so a peer that
        // streams an unbounded body is rejected before the buffer grows past the cap.
        let mut body: Vec<u8> = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| classify_loss(&error))? {
            if body.len() as u64 + chunk.len() as u64 > MAX_RECEIPT_BYTES {
                return Err(TransportError::Malformed);
            }
            body.extend_from_slice(&chunk);
        }
        let reply: ReceiptReply = serde_json::from_slice(&body).map_err(|_| TransportError::Malformed)?;
        Ok(Some(PeerReceipt {
            node: self.node.clone(),
            digest: digest.clone(),
            size: reply.size,
        }))
    }
}

/// The blob store a receipt endpoint reads durability from, behind its bearer credential.
#[derive(Clone)]
struct ReceiptHttpState {
    token: String,
    blobs: BlobStorage,
}

/// Build the authenticated peer-receipt endpoint over `blobs`, answering same-DC peers' placement-receipt
/// queries behind `token`.
///
/// # Errors
/// Returns [`HttpReceiptError::EmptyToken`] when the replication credential is empty.
pub fn receipt_router(token: impl Into<String>, blobs: impl Into<BlobStorage>) -> Result<Router, HttpReceiptError> {
    let token = token.into();
    if token.is_empty() {
        return Err(HttpReceiptError::EmptyToken);
    }
    Ok(Router::new()
        .route(RECEIPTS_ROUTE, get(serve_receipt))
        .with_state(ReceiptHttpState {
            token,
            blobs: blobs.into(),
        }))
}

async fn serve_receipt(
    State(state): State<ReceiptHttpState>,
    headers: HeaderMap,
    Path(encoded): Path<String>,
) -> Response {
    if !authorized(&headers, &state.token) {
        return unauthorized();
    }
    let Some(digest) = Digest::from_hex(&encoded) else {
        return (StatusCode::BAD_REQUEST, "invalid sha256 digest").into_response();
    };
    match state.blobs.head(&digest).await {
        Ok(Some(metadata)) => Json(ReceiptReply { size: metadata.bytes }).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
