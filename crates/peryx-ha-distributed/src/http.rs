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

use crate::protocol::{Change, ChangePage, PROTOCOL_VERSION, Primary};
use crate::replica::Replica;

/// A page limit for reading the replica's own durable state, where no page is fetched.
const ONE: NonZeroUsize = NonZeroUsize::new(1).expect("1 is non-zero");

const CHANGES_PATH: &str = "+replication/v1/changes";
const USER_AGENT: &str = concat!("peryx-ha-distributed/", env!("CARGO_PKG_VERSION"));

/// The largest change page the primary HTTP endpoint accepts.
pub const DEFAULT_MAX_CHANGE_PAGE_SIZE: usize = 1_000;

/// The most bytes a single change encodes to on the wire.
///
/// A change carries a base64 event payload, its metadata mutations, and its blob references. Base64
/// framing inflates by four bytes per three, so a 32 KiB budget covers a ~24 KiB raw event plus the
/// JSON keys, kebab-case operation tags, and blob digests around it. This is the protocol-level
/// per-field bound that lets a follower size the response it buffers without decoding it: without it
/// event payloads, metadata keys, and values are variable-length and a page's byte size is unbounded
/// even at a fixed record count.
const MAX_CHANGE_ENCODED_BYTES: u64 = 32 * 1024;

/// The JSON envelope around the change array: the `version`, `source`, `after`, and `current_serial`
/// fields plus the object and array punctuation, independent of the changes themselves.
const CHANGE_PAGE_ENVELOPE_BYTES: u64 = 4 * 1024;

/// The largest encoded change page a follower buffers before decoding it.
///
/// [`HttpPrimary::changes`] reads the primary's JSON response into memory before it can validate the
/// record count, so the byte size must be bounded on its own. A compromised primary, a wrong
/// endpoint, or a proxy can otherwise return an unbounded fixed-length or chunked body and exhaust
/// follower memory before validation runs. The bound is [`MAX_CHANGE_ENCODED_BYTES`] across a full
/// [`DEFAULT_MAX_CHANGE_PAGE_SIZE`] page plus the page's own [`CHANGE_PAGE_ENVELOPE_BYTES`] envelope,
/// so it always covers a valid page at the maximum record count and per-change size.
pub const DEFAULT_MAX_CHANGE_PAGE_BYTES: u64 =
    MAX_CHANGE_ENCODED_BYTES * DEFAULT_MAX_CHANGE_PAGE_SIZE as u64 + CHANGE_PAGE_ENVELOPE_BYTES;

/// The most artifact byte streams the primary serves at once.
///
/// A request that arrives while every slot is held earns a `503`, so a burst of slow, stalled, or
/// abandoned readers cannot exhaust the file handles, sockets, and buffers a stream pins. A finished,
/// cancelled, or disconnected reader frees its slot.
pub const DEFAULT_MAX_CONCURRENT_BLOB_STREAMS: NonZeroUsize = NonZeroUsize::new(32).expect("32 is non-zero");

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
    #[error("request primary: {0}")]
    Request(#[source] reqwest::Error),
    #[error("decode primary change page: {0}")]
    Decode(#[source] serde_json::Error),
    #[error("primary change page exceeds {limit} byte limit: {actual}")]
    ResponseTooLarge { limit: u64, actual: u64 },
}

/// A bearer-authenticated HTTP implementation of [`Primary`].
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
    /// Build a client rooted at the primary server URL.
    ///
    /// # Errors
    /// Returns an error for an empty token or an invalid HTTP(S) URL.
    ///
    /// # Panics
    /// Panics if the HTTP client cannot be built, which a static user agent over the guaranteed `rustls`
    /// provider never provokes.
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
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .expect("a reqwest client with a static user agent always builds");
        Ok(Self {
            http,
            changes_url,
            token,
            max_page_bytes: DEFAULT_MAX_CHANGE_PAGE_BYTES,
        })
    }

    #[cfg(test)]
    pub(crate) const fn with_max_page_bytes(mut self, max_page_bytes: u64) -> Self {
        self.max_page_bytes = max_page_bytes;
        self
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
        // A declared length lets the follower refuse an oversized body before reading a byte; a chunked
        // body without one is capped as it accumulates, so neither shape can exhaust follower memory.
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

/// Build the authenticated primary replication routes, bounding concurrent artifact streams at
/// [`DEFAULT_MAX_CONCURRENT_BLOB_STREAMS`].
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

/// Build the authenticated primary replication routes, bounding concurrent artifact streams at
/// `max_concurrent_streams`.
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

/// The state a follower-serving replica mounts to relay the change-feed. It carries no source of its own:
/// a replica serves the authoritative writer's stream, read from its durable [`ReplicaState`] at request
/// time, so a peer pulling from a replica or from the writer sees the same source and its apply path never
/// trips the single-source guard.
#[derive(Clone)]
struct FollowerHttpState {
    token: String,
    meta: MetaStore,
}

/// Build the authenticated change-feed route a read replica serves in follower mode.
///
/// The replica relays the writer's stream up to its own durably applied serial: its journal holds no
/// record past what it committed, so the page is bounded by construction, and each page carries the
/// authoritative source the replica mirrors rather than the replica's own identity.
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
        // A replica that has applied nothing yet knows no authoritative source and has no change to
        // relay; report it unavailable so a puller fails over to the writer or a caught-up peer.
        Ok(None) => return (StatusCode::SERVICE_UNAVAILABLE, "replica has not synced a source yet").into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    serve_change_page(&state.meta, &source, &query)
}

/// Serve one change page from `meta` after the requested serial, stamped with `source`. A replica's
/// journal reaches only its applied serial, so this is bounded by the replica's durable frontier without
/// a separate clamp; the writer's journal reaches its head.
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
                Ok(None) => return StatusCode::NOT_FOUND.into_response(),
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
        Err(error) if error.kind() == BlobErrorKind::NotFound => return StatusCode::NOT_FOUND.into_response(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    blob_content_response(read, partial, &digest, permit)
}

fn blob_content_response(read: BlobRead, partial: bool, digest: &Digest, permit: OwnedSemaphorePermit) -> Response {
    let mut builder = Response::builder()
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CACHE_CONTROL, "private, no-store")
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, format!("\"sha256:{}\"", digest.as_str()));
    if partial {
        builder = builder
            .status(StatusCode::PARTIAL_CONTENT)
            .header(header::CONTENT_LENGTH, read.range.end - read.range.start)
            .header(
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

/// The `503` a request earns when every stream slot is held, naming a retry delay and no internal detail.
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
            // `open` hands back a whole-file handle plus the selected range; position it so the body
            // streams only that range. A regular blob file seeks to an offset `open` already bounded to
            // its size, so this cannot fail in practice.
            file.seek(SeekFrom::Start(read.range.start))
                .expect("a stored blob file seeks to its validated range start");
            let length = read.range.end - read.range.start;
            let file = ReaderStream::new(tokio::fs::File::from_std(file).take(length));
            Body::from_stream(hold_permit(file, permit))
        }
        BlobReadBody::Stream(stream) => Body::from_stream(hold_permit(stream, permit)),
    }
}

/// Carry `permit` alongside `inner` so the stream slot stays reserved for exactly the body's lifetime.
/// The permit drops when the body finishes, the reader cancels, or the connection closes, whichever comes
/// first, releasing the slot for the next stream.
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
