use std::fmt;
use std::pin::Pin;

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use axum::routing::get;
use axum::{Json, Router};
use bytes::Bytes;
use futures_util::{Stream, TryStreamExt as _};
use peryx_storage::blob::{BlobErrorKind, BlobRead, BlobReadBody, BlobStorage, Digest};
use peryx_storage::meta::MetaStore;
use reqwest::Url;
use serde::Deserialize;
use tokio_util::io::ReaderStream;

use crate::protocol::{Change, ChangePage, PROTOCOL_VERSION, Primary};

const CHANGES_PATH: &str = "+replication/v1/changes";
const BLOBS_PATH: &str = "+replication/v1/blobs/sha256/";
const USER_AGENT: &str = concat!("peryx-replication/", env!("CARGO_PKG_VERSION"));

/// The largest change page the primary HTTP endpoint accepts.
pub const DEFAULT_MAX_CHANGE_PAGE_SIZE: usize = 1_000;

/// Invalid primary HTTP server configuration.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PrimaryHttpConfigError {
    #[error("primary source identity must not be empty")]
    EmptySource,
    #[error("primary replication token must not be empty")]
    EmptyToken,
}

/// An HTTP request, status, or response decoding failure.
#[derive(Debug, thiserror::Error)]
pub enum HttpPrimaryError {
    #[error("invalid primary URL {0:?}")]
    InvalidBase(String),
    #[error("primary replication token must not be empty")]
    EmptyToken,
    #[error("build replication HTTP client: {0}")]
    Client(#[source] reqwest::Error),
    #[error("request primary: {0}")]
    Request(#[source] reqwest::Error),
    #[error("decode primary change page: {0}")]
    Decode(#[source] serde_json::Error),
}

/// A bearer-authenticated HTTP implementation of [`Primary`].
#[derive(Clone)]
pub struct HttpPrimary {
    http: reqwest::Client,
    changes_url: Url,
    blobs_url: Url,
    token: String,
}

fn endpoint_url(base: &Url, path: &str) -> Url {
    let mut url = base.clone();
    url.set_path(&format!("{}{path}", base.path()));
    url
}

impl HttpPrimary {
    /// Build a client rooted at the primary server URL.
    ///
    /// # Errors
    /// Returns an error for an empty token, invalid HTTP(S) URL, or HTTP client construction failure.
    pub fn new(base: &str, token: impl Into<String>) -> Result<Self, HttpPrimaryError> {
        let token = token.into();
        if token.is_empty() {
            return Err(HttpPrimaryError::EmptyToken);
        }
        let Ok(mut base_url) = Url::parse(base) else {
            return Err(HttpPrimaryError::InvalidBase(base.to_owned()));
        };
        if !matches!(base_url.scheme(), "http" | "https") || base_url.cannot_be_a_base() {
            return Err(HttpPrimaryError::InvalidBase(base.to_owned()));
        }
        if !base_url.path().ends_with('/') {
            base_url.set_path(&format!("{}/", base_url.path()));
        }
        base_url.set_query(None);
        base_url.set_fragment(None);
        let changes_url = endpoint_url(&base_url, CHANGES_PATH);
        let blobs_url = endpoint_url(&base_url, BLOBS_PATH);
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .map_err(HttpPrimaryError::Client)?;
        Ok(Self {
            http,
            changes_url,
            blobs_url,
            token,
        })
    }
}

impl fmt::Debug for HttpPrimary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpPrimary")
            .field("changes_url", &self.changes_url)
            .field("blobs_url", &self.blobs_url)
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Primary for HttpPrimary {
    type Error = HttpPrimaryError;
    type BlobStream = Pin<Box<dyn Stream<Item = Result<Bytes, Self::Error>> + Send>>;

    async fn changes(&self, after: u64, limit: usize) -> Result<ChangePage, Self::Error> {
        let mut url = self.changes_url.clone();
        url.query_pairs_mut()
            .append_pair("after", &after.to_string())
            .append_pair("limit", &limit.to_string());
        let response = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(HttpPrimaryError::Request)?
            .error_for_status()
            .map_err(HttpPrimaryError::Request)?;
        let bytes = response.bytes().await.map_err(HttpPrimaryError::Request)?;
        serde_json::from_slice(&bytes).map_err(HttpPrimaryError::Decode)
    }

    async fn blob(&self, digest: &Digest) -> Result<Self::BlobStream, Self::Error> {
        let Ok(url) = self.blobs_url.join(digest.as_str()) else {
            return Err(HttpPrimaryError::InvalidBase(self.blobs_url.to_string()));
        };
        let response = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .header(header::ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(HttpPrimaryError::Request)?
            .error_for_status()
            .map_err(HttpPrimaryError::Request)?;
        Ok(Box::pin(response.bytes_stream().map_err(HttpPrimaryError::Request)))
    }
}

#[derive(Clone)]
struct PrimaryHttpState {
    source: String,
    token: String,
    meta: MetaStore,
    blobs: BlobStorage,
}

#[derive(Deserialize)]
struct ChangesQuery {
    after: u64,
    limit: usize,
}

/// Build the authenticated primary replication routes.
///
/// # Errors
/// Returns an error when the source identity or bearer token is empty.
pub fn primary_router(
    source: impl Into<String>,
    token: impl Into<String>,
    meta: MetaStore,
    blobs: impl Into<BlobStorage>,
) -> Result<Router, PrimaryHttpConfigError> {
    let source = source.into();
    if source.is_empty() {
        return Err(PrimaryHttpConfigError::EmptySource);
    }
    let token = token.into();
    if token.is_empty() {
        return Err(PrimaryHttpConfigError::EmptyToken);
    }
    Ok(Router::new()
        .route("/+replication/v1/changes", get(serve_changes))
        .route("/+replication/v1/blobs/sha256/{digest}", get(serve_blob))
        .with_state(PrimaryHttpState {
            source,
            token,
            meta,
            blobs: blobs.into(),
        }))
}

async fn serve_changes(
    State(state): State<PrimaryHttpState>,
    headers: HeaderMap,
    Query(query): Query<ChangesQuery>,
) -> Response {
    if !authorized(&headers, &state.token) {
        return unauthorized();
    }
    if query.limit == 0 || query.limit > DEFAULT_MAX_CHANGE_PAGE_SIZE {
        return (StatusCode::BAD_REQUEST, "change page limit is out of range").into_response();
    }
    match state.meta.journal_page_after(query.after, query.limit) {
        Ok((current_serial, records)) => Json(ChangePage {
            version: PROTOCOL_VERSION,
            source: state.source,
            after: query.after,
            current_serial,
            changes: records
                .into_iter()
                .map(|record| Change {
                    serial: record.serial,
                    event: record.payload,
                    metadata: record.mutations.into_iter().map(Into::into).collect(),
                    blobs: record.blobs.into_iter().map(Into::into).collect(),
                })
                .collect(),
        })
        .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn serve_blob(
    State(state): State<PrimaryHttpState>,
    headers: HeaderMap,
    Path(encoded): Path<String>,
) -> Response {
    if !authorized(&headers, &state.token) {
        return unauthorized();
    }
    let Some(digest) = Digest::from_hex(&encoded) else {
        return (StatusCode::BAD_REQUEST, "invalid sha256 digest").into_response();
    };
    let read = match state.blobs.open(&digest, None).await {
        Ok(read) => read,
        Err(error) if error.kind() == BlobErrorKind::NotFound => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CACHE_CONTROL, "private, no-store")
        .body(blob_body(read))
        .expect("static replication response headers are valid")
}

pub fn blob_body(read: BlobRead) -> Body {
    match read.body {
        BlobReadBody::File(file) => Body::from_stream(ReaderStream::new(tokio::fs::File::from_std(file))),
        BlobReadBody::Stream(stream) => Body::from_stream(stream),
    }
}

fn authorized(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|presented| constant_time_eq(presented.as_bytes(), expected.as_bytes()))
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0_u8, |difference, (left, right)| difference | (left ^ right))
            == 0
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer realm=\"peryx-replication\"")],
    )
        .into_response()
}
