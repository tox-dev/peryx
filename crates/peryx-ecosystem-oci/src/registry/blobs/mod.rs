//! Blob serving: local and proxied reads, HEAD, ingest and delete.
//!
//! Global blob deduplication requires repository-scoped links before reads.

mod contents;

use contents::{layer_contents_response, layer_query_member};
use peryx_driver::conditional::applicable_range;
use peryx_driver::range::{RangeSpec, parse_range, unsatisfiable_range};

use super::uploads::created;
use super::*;
use crate::error::{ErrorCode, error_response, gateway_error};
use crate::store::{self};
use crate::upstream::UpstreamError;
use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use futures_util::{Stream, TryStreamExt as _};
use peryx_driver::ServingState;
use peryx_events::metrics::Event;
use peryx_events::webhook::WebhookEventKind;
use peryx_index::Index;
use peryx_policy::PolicyAction;
use peryx_storage::blob::{BlobError, BlobErrorKind, BlobMetadata, BlobStorage, BlobWrite, Digest};
use std::sync::Arc;

impl<S: BuildHasher + Default + Send + Sync + 'static> OciRegistryWithHasher<S> {
    pub(super) async fn serve_blob(
        &self,
        state: &ServingState,
        name: &str,
        digest: &str,
        head: bool,
        headers: &HeaderMap,
    ) -> Result<Response, ServeError> {
        let Some((index, repo)) = resolve(&state.indexes, name) else {
            return Ok(error_response(ErrorCode::NameUnknown, "repository name unknown"));
        };
        if policy_blocks(index, PolicyAction::Serve, repo) {
            return Ok(error_response(ErrorCode::BlobUnknown, "blob unknown"));
        }
        let Some(storage) = store::blob_digest(digest) else {
            return Ok(error_response(
                ErrorCode::DigestInvalid,
                "only sha256 blob digests are supported",
            ));
        };
        // A blob is content-addressed, so its digest is the strong validator for its bytes.
        let etag = format!("\"{digest}\"");
        let asked = BlobRequest {
            range: applicable_range(headers, &etag),
            etag: &etag,
            head,
        };
        if head {
            return self.head_blob(state, index, repo, digest, &storage, &asked).await;
        }
        let mut response = match self.ensure_blob(state, index, repo, digest, &storage).await? {
            BlobFetch::Stored(metadata) => {
                serve_stored_blob(&state.blobs, &storage, digest, metadata.bytes, &asked).await?
            }
            BlobFetch::Absent => error_response(ErrorCode::BlobUnknown, "blob unknown"),
            BlobFetch::Gateway(response) => response,
        };
        if response.status().is_success() {
            let metrics = state.metrics.clone();
            let route = index.route.clone();
            let project = repo.to_owned();
            let filename = digest.to_owned();
            let body = std::mem::replace(response.body_mut(), Body::empty());
            *response.body_mut() = peryx_driver::body::on_body_complete(body, move |bytes| {
                metrics.record(Event::Download {
                    route,
                    project,
                    filename,
                    bytes,
                });
            });
        }
        Ok(response)
    }

    /// Answer a blob `HEAD`: from the store when cached, otherwise a cheap upstream `HEAD` on a proxy
    /// member so a client's pre-flight existence check never downloads the whole layer.
    async fn head_blob(
        &self,
        state: &ServingState,
        index: &Index,
        repo: &str,
        digest: &str,
        storage: &Digest,
        asked: &BlobRequest<'_>,
    ) -> Result<Response, ServeError> {
        if let Some(metadata) = state.blobs.head(storage).await.map_err(blob_fault)?
            && self.blob_authorized(state, index, repo, digest)?
        {
            return serve_stored_blob(&state.blobs, storage, digest, metadata.bytes, asked).await;
        }
        for member in serving_members(state, index) {
            let Some(client) = member.proxy_client() else {
                continue;
            };
            match self
                .upstream
                .blob_head(
                    client.base_url(),
                    client.auth(),
                    &self.upstream_repo(&member.name, client, repo),
                    digest,
                )
                .await
            {
                Ok(size) => {
                    store::record_blob_membership(&state.meta, &member.name, repo, digest)?;
                    return Ok(blob_head_response(digest, size, asked));
                }
                Err(UpstreamError::Status(status)) if absent_upstream(status) => {}
                Err(err) => return Ok(upstream_error_response(&err, "blob")),
            }
        }
        Ok(error_response(ErrorCode::BlobUnknown, "blob unknown"))
    }

    /// Make a blob present in the store, fetching it once through the single-flight gate on a miss.
    /// Concurrent misses for one content-addressed blob share the download: the first waiter fetches
    /// it, the rest wake to find it stored.
    async fn ensure_blob(
        &self,
        state: &ServingState,
        index: &Index,
        repo: &str,
        digest: &str,
        storage: &Digest,
    ) -> Result<BlobFetch, ServeError> {
        if let Some(metadata) = state.blobs.head(storage).await.map_err(blob_fault)?
            && self.blob_authorized(state, index, repo, digest)?
        {
            return Ok(BlobFetch::Stored(metadata));
        }
        let gate_key = format!("oci\0blob\0{digest}");
        let gate = flight_gate(state, &gate_key);
        let _guard = gate.lock().await;
        if let Some(metadata) = state.blobs.head(storage).await.map_err(blob_fault)?
            && self.blob_authorized(state, index, repo, digest)?
        {
            return Ok(BlobFetch::Stored(metadata));
        }
        let members = serving_members(state, index);
        let fetched = self.fetch_blob(state, &members, repo, digest, storage).await;
        state.cache.forget_flight(&gate_key);
        fetched
    }

    /// Serve `GET /v2/<name>/blobs/<digest>/contents`: list the tar members of a stored layer, or
    /// preview one text member. The layer is a (usually gzip) tar, so the same neutral archive engine
    /// drives it; the JSON listing and `text/plain` + `x-peryx-member-*` chunk headers follow the
    /// neutral archive-inspect contract, so the web UI's file browser renders a layer verbatim.
    pub(super) async fn serve_layer_contents(
        &self,
        state: &ServingState,
        name: &str,
        digest: &str,
        query: &str,
    ) -> Result<Response, ServeError> {
        let Some((index, repo)) = resolve(&state.indexes, name) else {
            return Ok(error_response(ErrorCode::NameUnknown, "repository name unknown"));
        };
        if policy_blocks(index, PolicyAction::Serve, repo) {
            return Ok(error_response(ErrorCode::BlobUnknown, "blob unknown"));
        }
        let Some(storage) = store::blob_digest(digest) else {
            return Ok(error_response(
                ErrorCode::DigestInvalid,
                "only sha256 blob digests are supported",
            ));
        };
        match self.ensure_blob(state, index, repo, digest, &storage).await? {
            BlobFetch::Stored(_) => {}
            BlobFetch::Absent => return Ok(error_response(ErrorCode::BlobUnknown, "blob unknown")),
            BlobFetch::Gateway(response) => return Ok(response),
        }
        let lease = state.blobs.materialize(&storage).await.map_err(blob_fault)?;
        let selected = layer_query_member(query);
        Ok(
            tokio::task::spawn_blocking(move || layer_contents_response(lease.path(), selected))
                .await
                .expect("layer inspection task panicked"),
        )
    }

    /// Fetch a missed blob from the first proxy member that has it, into the store. Called under the
    /// single-flight gate, so only one request per digest reaches an upstream.
    async fn fetch_blob(
        &self,
        state: &ServingState,
        members: &[&Index],
        repo: &str,
        digest: &str,
        storage: &Digest,
    ) -> Result<BlobFetch, ServeError> {
        let stored = state.blobs.head(storage).await.map_err(blob_fault)?;
        for member in members {
            let Some(client) = member.proxy_client() else {
                continue;
            };
            if let Some(metadata) = stored {
                match self
                    .upstream
                    .blob_head(
                        client.base_url(),
                        client.auth(),
                        &self.upstream_repo(&member.name, client, repo),
                        digest,
                    )
                    .await
                {
                    Ok(_) => {
                        store::record_blob_membership(&state.meta, &member.name, repo, digest)?;
                        return Ok(BlobFetch::Stored(metadata));
                    }
                    Err(UpstreamError::Status(status)) if absent_upstream(status) => continue,
                    Err(err) => return Ok(BlobFetch::Gateway(upstream_error_response(&err, "blob"))),
                }
            }
            match self
                .upstream
                .blob(
                    client.base_url(),
                    client.auth(),
                    &self.upstream_repo(&member.name, client, repo),
                    digest,
                )
                .await
            {
                Ok(response) => {
                    let bytes = match download_blob(&state.blobs, storage, response).await {
                        Ok(bytes) => bytes,
                        Err(err) => return Ok(BlobFetch::Gateway(download_error_response(err))),
                    };
                    store::record_blob_membership(&state.meta, &member.name, repo, digest)?;
                    return Ok(BlobFetch::Stored(BlobMetadata { bytes, modified: None }));
                }
                Err(UpstreamError::Status(status)) if absent_upstream(status) => {}
                Err(err) => {
                    return Ok(BlobFetch::Gateway(upstream_error_response(&err, "blob")));
                }
            }
        }
        Ok(BlobFetch::Absent)
    }

    pub(super) fn delete_blob(
        &self,
        state: &Arc<ServingState>,
        headers: &HeaderMap,
        name: &str,
        digest: &str,
    ) -> Result<Response, ServeError> {
        let (index, repo, identity) = match resolve_writable(state, name, headers, Action::Delete) {
            Ok(target) => target,
            Err(response) => return Ok(response),
        };
        if store::blob_digest(digest).is_none() {
            return Ok(error_response(
                ErrorCode::DigestInvalid,
                "only sha256 blob digests are supported",
            ));
        }
        let membership = store::blob_membership_key(&index.name, &repo, digest);
        let deleted = {
            let mut memberships = self.blob_memberships.write();
            memberships.remove(&membership);
            let deleted = store::delete_blob_membership(&state.meta, &index.name, &repo, digest)?;
            drop(memberships);
            deleted
        };
        if !deleted {
            return Ok(error_response(ErrorCode::BlobUnknown, "blob unknown"));
        }
        emit_webhook(
            state,
            &Requester {
                headers,
                identity: &identity,
            },
            WebhookEventKind::Delete,
            index,
            &repo,
            None,
            Some(digest.to_owned()),
        );
        Ok(accepted())
    }

    pub(super) fn blob_authorized(
        &self,
        state: &ServingState,
        index: &Index,
        repo: &str,
        digest: &str,
    ) -> Result<bool, ServeError> {
        if !matches!(index.kind, IndexKind::Virtual { .. }) {
            return self.blob_is_member(state, &index.name, repo, digest);
        }
        for member in serving_members(state, index) {
            if self.blob_is_member(state, &member.name, repo, digest)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn blob_is_member(&self, state: &ServingState, index: &str, repo: &str, digest: &str) -> Result<bool, ServeError> {
        let key = store::blob_membership_key(index, repo, digest);
        if self.blob_memberships.read().contains(&key) {
            return Ok(true);
        }
        let mut memberships = self.blob_memberships.write();
        let cached = memberships.contains(&key);
        let present = cached || store::blob_is_member(&state.meta, index, repo, digest)?;
        if present && !cached {
            memberships.insert(key);
        }
        drop(memberships);
        Ok(present)
    }
}

/// The outcome of fetching a missed blob from a virtual index's proxy members.
enum BlobFetch {
    /// The blob was fetched from an upstream and is now in the store.
    Stored(BlobMetadata),
    /// No proxy member has the blob; the client gets a `404`.
    Absent,
    /// A member erred mid-fetch; this ready gateway response carries the reason.
    Gateway(Response),
}

/// A failed blob ingest: the store rejected it (digest mismatch or io) or the transfer errored.
#[derive(Debug)]
pub enum DownloadError {
    Blob(BlobError),
    Stream(String),
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Blob(err) => write!(formatter, "blob store error: {err}"),
            Self::Stream(err) => write!(formatter, "blob body read failed: {err}"),
        }
    }
}

impl std::error::Error for DownloadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Blob(err) => Some(err),
            Self::Stream(_) => None,
        }
    }
}

impl From<BlobError> for DownloadError {
    fn from(err: BlobError) -> Self {
        Self::Blob(err)
    }
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "passed as a function pointer to `map_err`, which hands over the owned error"
)]
pub(super) fn blob_fault(err: BlobError) -> ServeError {
    ServeError::Transport(err.to_string())
}

/// Stream an upstream blob into the store, verifying its digest on commit.
pub async fn download_blob(
    blobs: &BlobStorage,
    storage: &Digest,
    response: reqwest::Response,
) -> Result<u64, DownloadError> {
    let stream = response.bytes_stream().map_err(|err| err.to_string());
    ingest_blob(blobs, storage, stream).await
}

/// Drain a byte stream into a staged blob and commit it under `storage`. Takes the transfer error
/// pre-stringified so this stays one instantiation a test can drive with a plain-string failure.
async fn ingest_blob(
    blobs: &BlobStorage,
    storage: &Digest,
    stream: impl Stream<Item = Result<bytes::Bytes, String>> + Send,
) -> Result<u64, DownloadError> {
    let mut pending = blobs.begin().await?;
    let mut stream = std::pin::pin!(stream);
    let mut bytes = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                return Err(match pending.abort().await {
                    Ok(()) => DownloadError::Stream(error),
                    Err(cleanup) => DownloadError::Blob(cleanup),
                });
            }
        };
        bytes = bytes.saturating_add(chunk.len() as u64);
        pending.write_chunk(chunk).await?;
    }
    pending.commit(storage).await?;
    Ok(bytes)
}

/// Map a failed ingest to a client response: a digest mismatch is the client's fault, the rest ours.
fn download_error_response(err: DownloadError) -> Response {
    match err {
        DownloadError::Blob(err) if err.kind() == BlobErrorKind::DigestMismatch => {
            let (expected, actual) = err.mismatch().expect("digest mismatch carries both digests");
            error_response(
                ErrorCode::DigestInvalid,
                &format!("blob digest mismatch: expected {expected}, got {actual}"),
            )
        }
        DownloadError::Blob(err) => gateway_error(&format!("blob store error: {err}")),
        DownloadError::Stream(err) => gateway_error(&format!("blob body read failed: {err}")),
    }
}

pub(super) async fn commit_blob(
    state: &ServingState,
    pending: BlobWrite,
    index: &str,
    repo: &str,
    name: &str,
    digest: &str,
) -> Result<Response, ServeError> {
    let Some(storage) = store::blob_digest(digest) else {
        return Ok(error_response(
            ErrorCode::DigestInvalid,
            "only sha256 blob digests are supported",
        ));
    };
    match pending.commit(&storage).await {
        Ok(()) => {
            store::record_blob_membership(&state.meta, index, repo, digest)?;
            Ok(blob_created(name, digest))
        }
        Err(err) => Ok(download_error_response(DownloadError::Blob(err))),
    }
}

/// `201 Created` for a stored blob, with its location and digest.
pub(super) fn blob_created(name: &str, digest: &str) -> Response {
    created(&format!("/v2/{name}/blobs/{digest}"), digest)
}

/// What a client asked of a blob, once `If-Range` has had its say on the range.
struct BlobRequest<'a> {
    /// The blob's entity tag, sent with every response so a client has a validator to condition on.
    etag: &'a str,
    /// The single range to serve, or `None` for the whole blob.
    range: Option<&'a str>,
    head: bool,
}

/// Stream a stored blob, honoring a single-range request with `206`/`Content-Range`.
async fn serve_stored_blob(
    blobs: &BlobStorage,
    storage: &Digest,
    digest: &str,
    size: u64,
    asked: &BlobRequest<'_>,
) -> Result<Response, ServeError> {
    let common = [
        (header::CONTENT_TYPE, HeaderValue::from_static(OCTET_STREAM)),
        (header::ACCEPT_RANGES, HeaderValue::from_static("bytes")),
        (header::ETAG, header_value(asked.etag)),
        (DOCKER_CONTENT_DIGEST, header_value(digest)),
    ];
    let spec = asked.range.map_or(RangeSpec::Ignore, |value| parse_range(value, size));
    let (start, end) = match spec {
        RangeSpec::Ignore => {
            let mut builder = Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_LENGTH, size);
            for (name, value) in common {
                builder = builder.header(name, value);
            }
            let body = if asked.head {
                Body::empty()
            } else {
                peryx_driver::body::blob_read(blobs.open(storage, None).await.map_err(blob_fault)?)
            };
            return Ok(builder
                .body(body)
                .expect("blob response builds from validated header parts"));
        }
        RangeSpec::Unsatisfiable => return Ok(unsatisfiable_range(size)),
        RangeSpec::Satisfiable(start, end) => (start, end),
    };
    let length = end - start + 1;
    let mut builder = Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(header::CONTENT_LENGTH, length)
        .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{size}"));
    for (name, value) in common {
        builder = builder.header(name, value);
    }
    if asked.head {
        return Ok(builder.body(Body::empty()).expect("range head response builds"));
    }
    Ok(builder
        .body(peryx_driver::body::blob_read(
            blobs.open(storage, Some(start..end + 1)).await.map_err(blob_fault)?,
        ))
        .expect("range response builds from validated header parts"))
}

/// A blob `HEAD` response: the size and digest headers a client needs to decide whether to pull, with
/// no body.
fn blob_head_response(digest: &str, size: Option<u64>, asked: &BlobRequest<'_>) -> Response {
    // A `HEAD` answers a `Range` the way the matching `GET` would. Ignoring it here while honouring
    // it for a cached blob made one request give two answers depending on what the store happened to
    // hold, which is the one thing a client checking a layer must not see.
    let (status, length, content_range) = match size {
        None => (StatusCode::OK, None, None),
        Some(size) => match asked.range.map_or(RangeSpec::Ignore, |value| parse_range(value, size)) {
            RangeSpec::Ignore => (StatusCode::OK, Some(size), None),
            RangeSpec::Unsatisfiable => return unsatisfiable_range(size),
            RangeSpec::Satisfiable(start, end) => (
                StatusCode::PARTIAL_CONTENT,
                Some(end - start + 1),
                Some(format!("bytes {start}-{end}/{size}")),
            ),
        },
    };
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, OCTET_STREAM)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, header_value(asked.etag))
        .header(DOCKER_CONTENT_DIGEST, header_value(digest));
    if let Some(length) = length {
        builder = builder.header(header::CONTENT_LENGTH, length);
    }
    if let Some(content_range) = content_range {
        builder = builder.header(header::CONTENT_RANGE, content_range);
    }
    let body = length.map_or_else(
        || Body::from_stream(futures_util::stream::empty::<Result<bytes::Bytes, std::io::Error>>()),
        |_| Body::empty(),
    );
    builder
        .body(body)
        .expect("blob head response builds from validated parts")
}

fn header_value(value: &str) -> HeaderValue {
    HeaderValue::from_str(value).unwrap_or(HeaderValue::from_static(""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_download_error_maps_mismatch_to_client_and_the_rest_to_gateway() {
        let mismatch = DownloadError::Blob(BlobError::digest_mismatch(&Digest::of(b"a"), &Digest::of(b"b")));
        assert_eq!(download_error_response(mismatch).status(), StatusCode::BAD_REQUEST);
        let io = DownloadError::Blob(BlobError::io(std::io::Error::other("disk")));
        assert_eq!(download_error_response(io).status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            download_error_response(DownloadError::Stream("reset".to_owned())).status(),
            StatusCode::BAD_GATEWAY
        );
    }

    #[test]
    fn test_download_blob_error_reports_a_source() {
        use std::error::Error as _;

        assert!(
            DownloadError::Blob(BlobError::io(std::io::Error::other("disk")))
                .source()
                .is_some()
        );
    }

    #[test]
    fn test_download_stream_error_has_no_source() {
        use std::error::Error as _;

        assert!(DownloadError::Stream("reset".to_owned()).source().is_none());
    }

    #[test]
    fn test_blob_fault_is_a_transport_error() {
        assert!(matches!(
            blob_fault(BlobError::not_found(&Digest::of(b"x"))),
            ServeError::Transport(_)
        ));
    }

    #[tokio::test]
    async fn test_ingest_blob_reports_a_stream_error() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = BlobStorage::filesystem(dir.path().join("blobs"));
        let storage = Digest::of(b"x");
        let stream = futures_util::stream::iter(vec![Err("boom".to_owned())]);
        let err = ingest_blob(&blobs, &storage, stream).await.unwrap_err();
        assert!(matches!(err, DownloadError::Stream(message) if message == "boom"));
    }

    #[tokio::test]
    async fn test_ingest_blob_reports_a_cleanup_error() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("blobs");
        let blobs = BlobStorage::filesystem(&root);
        let storage = Digest::of(b"x");
        let stream = futures_util::stream::once(async move {
            let stage = std::fs::read_dir(&root).unwrap().next().unwrap().unwrap().path();
            std::fs::remove_file(&stage).unwrap();
            std::fs::create_dir(&stage).unwrap();
            Err("boom".to_owned())
        });
        let err = ingest_blob(&blobs, &storage, stream).await.unwrap_err();
        assert!(matches!(err, DownloadError::Blob(_)));
    }
}
