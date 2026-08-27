use std::fmt;
use std::io::{Seek as _, SeekFrom};
use std::num::NonZeroUsize;
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::Stream;
use peryx_storage::blob::{BlobErrorKind, BlobRead, BlobReadBody, BlobStorage, Digest, RangeRequest, parse_range};
use peryx_storage::meta::MetaStore;
use reqwest::Url;
use serde::Deserialize;
use tokio::io::AsyncReadExt as _;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::io::ReaderStream;

use crate::blob_http::{BLOB_MISS_HEADER, BLOB_MISS_VALUE};
use crate::protocol::{Change, ChangePage, PROTOCOL_VERSION, Primary};
use crate::replica::Replica;

/// Reads replica state without fetching journal records.
const ONE: NonZeroUsize = NonZeroUsize::new(1).expect("1 is non-zero");

const CHANGES_PATH: &str = "+replication/v1/changes";
const USER_AGENT: &str = concat!("peryx-ha-distributed/", env!("CARGO_PKG_VERSION"));

pub const DEFAULT_MAX_CHANGE_PAGE_SIZE: usize = 1_000;

/// A 32 KiB budget covers a base64-encoded 24 KiB event and its JSON metadata. This per-change bound
/// lets followers cap a page before decoding variable-length fields.
const MAX_CHANGE_ENCODED_BYTES: u64 = 32 * 1024;

/// Headroom for page fields and JSON framing outside the change array.
const CHANGE_PAGE_ENVELOPE_BYTES: u64 = 4 * 1024;

/// [`HttpPrimary::changes`] buffers JSON before validating record count, so it must cap fixed-length
/// and chunked bodies separately. The bound covers a full page at the per-change limit plus JSON
/// framing.
pub const DEFAULT_MAX_CHANGE_PAGE_BYTES: u64 =
    MAX_CHANGE_ENCODED_BYTES * DEFAULT_MAX_CHANGE_PAGE_SIZE as u64 + CHANGE_PAGE_ENVELOPE_BYTES;

/// Bounds file handles, sockets, and buffers held by slow artifact readers. Requests receive `503`
/// while all slots are occupied; completion, cancellation, and disconnect release a slot.
pub const DEFAULT_MAX_CONCURRENT_BLOB_STREAMS: NonZeroUsize = NonZeroUsize::new(32).expect("32 is non-zero");

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PrimaryHttpConfigError {
    #[error("primary source identity must not be empty")]
    EmptySource,
    #[error("primary replication token must not be empty")]
    EmptyToken,
}

#[derive(Debug, thiserror::Error)]
pub enum HttpPrimaryError {
    #[error("invalid primary URL {0:?}")]
    InvalidBase(String),
    #[error("primary replication token must not be empty")]
    EmptyToken,
    #[error("request primary: {0}")]
    Request(#[source] reqwest::Error),
    #[error("decode primary change page: {0}")]
    Decode(#[source] serde_json::Error),
    #[error("primary change page exceeds {limit} byte limit: {actual}")]
    ResponseTooLarge { limit: u64, actual: u64 },
}

#[derive(Clone)]
pub struct HttpPrimary {
    http: reqwest::Client,
    changes_url: Url,
    token: String,
    max_page_bytes: u64,
}

fn endpoint_url(base: &Url, path: &str) -> Url {
    let mut url = base.clone();
    url.set_path(&format!("{}{path}", base.path()));
    url
}

impl HttpPrimary {
    /// # Errors
    /// Returns an error for an empty token or an invalid HTTP(S) URL.
    ///
    /// # Panics
    /// Panics if reqwest rejects the static user agent.
    pub fn new(base: &str, token: impl Into<String>) -> Result<Self, HttpPrimaryError> {
        Self::build(base, token, DEFAULT_MAX_CHANGE_PAGE_BYTES)
    }

    fn build(base: &str, token: impl Into<String>, max_page_bytes: u64) -> Result<Self, HttpPrimaryError> {
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
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .expect("a reqwest client with a static user agent always builds");
        Ok(Self {
            http,
            changes_url,
            token,
            max_page_bytes,
        })
    }
}

impl fmt::Debug for HttpPrimary {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpPrimary")
            .field("changes_url", &self.changes_url)
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl Primary for HttpPrimary {
    type Error = HttpPrimaryError;

    async fn changes(&self, after: u64, limit: usize) -> Result<ChangePage, Self::Error> {
        let mut url = self.changes_url.clone();
        url.query_pairs_mut()
            .append_pair("after", &after.to_string())
            .append_pair("limit", &limit.to_string());
        let mut response = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(HttpPrimaryError::Request)?
            .error_for_status()
            .map_err(HttpPrimaryError::Request)?;
        let cap = self.max_page_bytes;
        // Reject declared sizes before reading; cap chunked bodies while accumulating them.
        if let Some(length) = response.content_length()
            && length > cap
        {
            return Err(HttpPrimaryError::ResponseTooLarge {
                limit: cap,
                actual: length,
            });
        }
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(HttpPrimaryError::Request)? {
            let total = bytes.len() as u64 + chunk.len() as u64;
            if total > cap {
                return Err(HttpPrimaryError::ResponseTooLarge {
                    limit: cap,
                    actual: total,
                });
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(HttpPrimaryError::Decode)
    }
}

#[derive(Clone)]
struct PrimaryHttpState {
    source: String,
    token: String,
    meta: MetaStore,
    blobs: BlobStorage,
    stream_permits: Arc<Semaphore>,
}

#[derive(Deserialize)]
struct ChangesQuery {
    after: u64,
    limit: usize,
}

/// Bounds concurrent artifact streams at [`DEFAULT_MAX_CONCURRENT_BLOB_STREAMS`].
///
/// # Errors
/// Returns an error when the source identity or bearer token is empty.
pub fn primary_router(
    source: impl Into<String>,
    token: impl Into<String>,
    meta: MetaStore,
    blobs: impl Into<BlobStorage>,
) -> Result<Router, PrimaryHttpConfigError> {
    primary_router_with_stream_limit(source, token, meta, blobs, DEFAULT_MAX_CONCURRENT_BLOB_STREAMS)
}

/// Bounds concurrent artifact streams at `max_concurrent_streams`.
///
/// # Errors
/// Returns an error when the source identity or bearer token is empty.
pub fn primary_router_with_stream_limit(
    source: impl Into<String>,
    token: impl Into<String>,
    meta: MetaStore,
    blobs: impl Into<BlobStorage>,
    max_concurrent_streams: NonZeroUsize,
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
            stream_permits: Arc::new(Semaphore::new(max_concurrent_streams.get())),
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
    serve_change_page(&state.meta, &state.source, &query)
}

/// Relays the authoritative writer's identity from durable replica state, preserving the single-source
/// invariant for peers that fetch through a replica.
#[derive(Clone)]
struct FollowerHttpState {
    token: String,
    meta: MetaStore,
}

/// Relays the writer's stream and identity through the replica's durable frontier.
///
/// # Errors
/// Returns an error when the bearer token is empty.
pub fn follower_router(token: impl Into<String>, meta: MetaStore) -> Result<Router, PrimaryHttpConfigError> {
    let token = token.into();
    if token.is_empty() {
        return Err(PrimaryHttpConfigError::EmptyToken);
    }
    Ok(Router::new()
        .route("/+replication/v1/changes", get(serve_follower_changes))
        .with_state(FollowerHttpState { token, meta }))
}

async fn serve_follower_changes(
    State(state): State<FollowerHttpState>,
    headers: HeaderMap,
    Query(query): Query<ChangesQuery>,
) -> Response {
    if !authorized(&headers, &state.token) {
        return unauthorized();
    }
    let source = match Replica::new(&state.meta, ONE).state() {
        Ok(Some(applied)) => applied.source,
        // Without a durable source identity, fail over instead of relaying an ambiguous stream.
        Ok(None) => return (StatusCode::SERVICE_UNAVAILABLE, "replica has not synced a source yet").into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    serve_change_page(&state.meta, &source, &query)
}

/// Replica journals end at their durable frontier, so relayed pages need no separate frontier clamp.
fn serve_change_page(meta: &MetaStore, source: &str, query: &ChangesQuery) -> Response {
    if query.limit == 0 || query.limit > DEFAULT_MAX_CHANGE_PAGE_SIZE {
        return (StatusCode::BAD_REQUEST, "change page limit is out of range").into_response();
    }
    match meta.journal_page_after(query.after, query.limit) {
        Ok((current_serial, records)) => Json(ChangePage {
            version: PROTOCOL_VERSION,
            source: source.to_owned(),
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
    let Ok(permit) = Arc::clone(&state.stream_permits).try_acquire_owned() else {
        return at_capacity();
    };
    let requested = match headers.get(header::RANGE).and_then(|value| value.to_str().ok()) {
        None => None,
        Some(range) => {
            let size = match state.blobs.head(&digest).await {
                Ok(Some(metadata)) => metadata.bytes,
                Ok(None) => return blob_not_found(),
                Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            };
            match parse_range(Some(range), size) {
                RangeRequest::Whole => None,
                RangeRequest::Unsatisfiable => return unsatisfiable_range(size),
                RangeRequest::Partial(range) => Some(range),
            }
        }
    };
    let partial = requested.is_some();
    let read = match state.blobs.open(&digest, requested).await {
        Ok(read) => read,
        Err(error) if error.kind() == BlobErrorKind::NotFound => return blob_not_found(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    blob_content_response(read, partial, &digest, permit)
}

fn blob_not_found() -> Response {
    (StatusCode::NOT_FOUND, [(BLOB_MISS_HEADER, BLOB_MISS_VALUE)]).into_response()
}

fn blob_content_response(read: BlobRead, partial: bool, digest: &Digest, permit: OwnedSemaphorePermit) -> Response {
    let mut builder = Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, read.range.end - read.range.start)
        .header(header::CACHE_CONTROL, "private, no-store")
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, format!("\"sha256:{}\"", digest.as_str()));
    if partial {
        builder = builder.status(StatusCode::PARTIAL_CONTENT).header(
            header::CONTENT_RANGE,
            format!(
                "bytes {}-{}/{}",
                read.range.start,
                read.range.end - 1,
                read.metadata.bytes
            ),
        );
    }
    builder
        .body(blob_body(read, permit))
        .expect("replication blob response headers are valid")
}

fn at_capacity() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::RETRY_AFTER, "1")],
        "peer artifact stream capacity reached",
    )
        .into_response()
}

fn unsatisfiable_range(size: u64) -> Response {
    Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_RANGE, format!("bytes */{size}"))
        .body(Body::empty())
        .expect("replication range response headers are valid")
}

pub fn blob_body(read: BlobRead, permit: OwnedSemaphorePermit) -> Body {
    match read.body {
        BlobReadBody::File(mut file) => {
            // `open` validates the selected range but returns a whole-file handle, so seek before
            // limiting the stream to the range length.
            file.seek(SeekFrom::Start(read.range.start))
                .expect("a stored blob file seeks to its validated range start");
            let length = read.range.end - read.range.start;
            let file = ReaderStream::new(tokio::fs::File::from_std(file).take(length));
            Body::from_stream(hold_permit(file, permit))
        }
        BlobReadBody::Stream(stream) => Body::from_stream(hold_permit(stream, permit)),
    }
}

/// Holds the stream permit until completion, cancellation, or disconnect.
fn hold_permit<S>(inner: S, permit: OwnedSemaphorePermit) -> impl Stream<Item = S::Item> + Send
where
    S: Stream + Send + 'static,
    S::Item: Send,
{
    let mut inner = Box::pin(inner);
    futures_util::stream::poll_fn(move |context| {
        let _slot = &permit;
        inner.as_mut().poll_next(context)
    })
}

pub fn authorized(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|presented| constant_time_eq(presented.as_bytes(), expected.as_bytes()))
}

pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0_u8, |difference, (left, right)| difference | (left ^ right))
            == 0
}

pub fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer realm=\"peryx-ha-distributed\"")],
    )
        .into_response()
}

#[cfg(test)]
#[path = "../tests/unit/http_tests.rs"]
mod tests;
