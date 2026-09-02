//! Same-datacenter peers query whether the local filesystem has a durable copy of a digest. A committed
//! [`head`](peryx_storage::blob::BlobStorage::head) hit returns `200` with the serving node, the digest,
//! and the byte length; a miss returns `404` and remains eligible for polling. The endpoint uses the
//! replication bearer credential.
//!
//! The shared credential proves group membership, not which member answered, so the client holds every
//! `200` to the node its source was configured for. Two sources aimed at one process therefore yield one
//! receipt instead of two.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use peryx_storage::blob::{BlobStorage, Digest};
use reqwest::Url;
use serde::{Deserialize, Serialize};

use crate::client_transport::{
    HttpClientConfigError, HttpClientTransport, ReplicationStatus, classify_status, replication_error,
    require_replication_success,
};
use crate::http::{authorized, unauthorized};
use crate::peer::TransportError;
use crate::peer_receipt::{PeerReceipt, ReceiptRequest, ReceiptSource};

const RECEIPTS_PATH: &str = "+replication/v1/receipts/sha256/";
const RECEIPTS_ROUTE: &str = "/+replication/v1/receipts/sha256/{digest}";
// Caps receipt bodies to prevent unbounded peer responses.
const MAX_RECEIPT_BYTES: u64 = 4096;

/// A `200` names the node that read its own store, the digest it read, and that blob's durable byte
/// length, so the caller can hold the answer to the peer it asked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptReply {
    pub node: String,
    pub digest: String,
    pub size: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum HttpReceiptError {
    #[error("peer replication token must not be empty")]
    EmptyToken,
    #[error("peer node identity must not be empty")]
    EmptyNode,
    #[error("invalid peer URL {0:?}")]
    InvalidBase(String),
}

#[derive(Clone)]
pub struct HttpReceiptSource {
    http: HttpClientTransport,
    receipts_url: Url,
    node: String,
}

impl fmt::Debug for HttpReceiptSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpReceiptSource")
            .field("receipts_url", &self.receipts_url)
            .field("node", &self.node)
            .field("http", &self.http)
            .finish_non_exhaustive()
    }
}

impl HttpReceiptSource {
    /// # Errors
    /// Returns [`HttpReceiptError`] for an empty token or node, or a URL that is not a usable HTTP(S)
    /// base.
    ///
    /// # Panics
    /// Panics if reqwest client construction fails.
    pub fn new(
        base: &str,
        node: impl Into<String>,
        token: impl Into<String>,
        timeout: Duration,
    ) -> Result<Self, HttpReceiptError> {
        let http = HttpClientTransport::new(base, token, timeout).map_err(map_config_error)?;
        let node = node.into();
        if node.is_empty() {
            return Err(HttpReceiptError::EmptyNode);
        }
        let receipts_url = http.endpoint(RECEIPTS_PATH);
        Ok(Self {
            http,
            receipts_url,
            node,
        })
    }
}

#[async_trait]
impl ReceiptSource for HttpReceiptSource {
    fn node(&self) -> &str {
        &self.node
    }

    async fn fetch_receipt(&self, request: ReceiptRequest<'_>) -> Result<Option<PeerReceipt>, TransportError> {
        let url = format!("{}{}", self.receipts_url, request.digest.as_str());
        let response = self
            .http
            .send(
                self.http
                    .get(Url::parse(&url).expect("a digest extends a valid receipt URL")),
            )
            .await
            .map_err(replication_error)?;
        if classify_status(response.status()) == ReplicationStatus::NotFound {
            return Ok(None);
        }
        require_replication_success(response.status(), response.headers())?;
        let body = self.http.read_small_body(response, MAX_RECEIPT_BYTES).await?;
        let reply: ReceiptReply = serde_json::from_slice(&body).map_err(|_| TransportError::Malformed)?;
        let digest = Digest::from_hex(&reply.digest).ok_or(TransportError::Malformed)?;
        let receipt = PeerReceipt {
            node: reply.node,
            digest,
            size: reply.size,
        };
        receipt.verify(&self.node, request)?;
        Ok(Some(receipt))
    }
}

fn map_config_error(error: HttpClientConfigError) -> HttpReceiptError {
    match error {
        HttpClientConfigError::EmptyToken => HttpReceiptError::EmptyToken,
        HttpClientConfigError::InvalidBase(base) => HttpReceiptError::InvalidBase(base),
    }
}

#[derive(Clone)]
struct ReceiptHttpState {
    token: String,
    node: String,
    blobs: BlobStorage,
}

/// Serves receipts that attest `node`, the identity peers know this process by.
///
/// # Errors
/// Returns [`HttpReceiptError::EmptyToken`] when the replication credential is empty, or
/// [`HttpReceiptError::EmptyNode`] when the serving identity is empty.
pub fn receipt_router(
    token: impl Into<String>,
    node: impl Into<String>,
    blobs: impl Into<BlobStorage>,
) -> Result<Router, HttpReceiptError> {
    let token = token.into();
    if token.is_empty() {
        return Err(HttpReceiptError::EmptyToken);
    }
    let node = node.into();
    if node.is_empty() {
        return Err(HttpReceiptError::EmptyNode);
    }
    Ok(Router::new()
        .route(RECEIPTS_ROUTE, get(serve_receipt))
        .with_state(ReceiptHttpState {
            token,
            node,
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
        Ok(Some(metadata)) => Json(ReceiptReply {
            node: state.node,
            digest: digest.as_str().to_owned(),
            size: metadata.bytes,
        })
        .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
