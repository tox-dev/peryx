//! Same-datacenter peers query whether the local filesystem has a durable copy of a digest. A committed
//! [`head`](peryx_storage::blob::BlobStorage::head) hit returns `200` with the byte length; a miss returns
//! `404` and remains eligible for polling. The endpoint uses the replication bearer credential.

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
use crate::peer_receipt::{PeerReceipt, ReceiptSource};

const RECEIPTS_PATH: &str = "+replication/v1/receipts/sha256/";
const RECEIPTS_ROUTE: &str = "/+replication/v1/receipts/sha256/{digest}";
// Caps receipt bodies to prevent unbounded peer responses.
const MAX_RECEIPT_BYTES: u64 = 4096;

/// The `200` response confirms the requested digest, so the body carries only its durable byte length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptReply {
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

    async fn fetch_receipt(&self, digest: &Digest) -> Result<Option<PeerReceipt>, TransportError> {
        let url = format!("{}{}", self.receipts_url, digest.as_str());
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
        require_replication_success(response.status())?;
        let body = self.http.read_small_body(response, MAX_RECEIPT_BYTES).await?;
        let reply: ReceiptReply = serde_json::from_slice(&body).map_err(|_| TransportError::Malformed)?;
        Ok(Some(PeerReceipt {
            node: self.node.clone(),
            digest: digest.clone(),
            size: reply.size,
        }))
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
    blobs: BlobStorage,
}

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
