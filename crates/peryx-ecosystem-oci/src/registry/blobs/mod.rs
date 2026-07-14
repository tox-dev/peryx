//! Blob serving: local and proxied reads, HEAD, ingest and delete.
//!
//! Global blob deduplication requires repository-scoped links before reads.

mod contents;
mod range;

use contents::{layer_contents_response, layer_query_member};
use range::{RangeSpec, parse_range, unsatisfiable_range};

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
use peryx_storage::blob::{BlobError, BlobStore, Digest, PendingBlob};
use std::sync::Arc;

impl OciRegistry {
    pub(super) async fn serve_blob(
        &self,
        state: &ServingState,
        name: &str,
        digest: &str,
        head: bool,
        range: Option<&str>,
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
        if head {
            return self.head_blob(state, index, repo, digest, &storage, range).await;
        }
        let response = match self.ensure_blob(state, index, repo, digest, &storage).await? {
            BlobFetch::Stored => serve_stored_blob(&state.blobs, &storage, digest, false, range).await?,
            BlobFetch::Absent => error_response(ErrorCode::BlobUnknown, "blob unknown"),
            BlobFetch::Gateway(response) => response,
        };
        // A blob served to a GET is a download (a HEAD returned earlier, above).
        if response.status().is_success() {
            state.metrics.record(Event::Download {
                route: index.route.clone(),
                project: repo.to_owned(),
                filename: digest.to_owned(),
                bytes: served_bytes(&response),
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
        range: Option<&str>,
    ) -> Result<Response, ServeError> {
        if state.blobs.exists(storage) && self.blob_authorized(state, index, repo, digest)? {
            return serve_stored_blob(&state.blobs, storage, digest, true, range).await;
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
                    return Ok(blob_head_response(digest, size, range));
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
        if state.blobs.exists(storage) && self.blob_authorized(state, index, repo, digest)? {
            return Ok(BlobFetch::Stored);
        }
        let gate_key = format!("oci\0blob\0{digest}");
        let gate = flight_gate(state, &gate_key);
        let _guard = gate.lock().await;
        if state.blobs.exists(storage) && self.blob_authorized(state, index, repo, digest)? {
            return Ok(BlobFetch::Stored);
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
            BlobFetch::Stored => {}
            BlobFetch::Absent => return Ok(error_response(ErrorCode::BlobUnknown, "blob unknown")),
            BlobFetch::Gateway(response) => return Ok(response),
        }
        let path = state.blobs.path_for(&storage);
        let selected = layer_query_member(query);
        Ok(
            tokio::task::spawn_blocking(move || layer_contents_response(&path, selected))
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
        let stored = state.blobs.exists(storage);
        for member in members {
            let Some(client) = member.proxy_client() else {
                continue;
            };
            if stored {
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
                        return Ok(BlobFetch::Stored);
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
                    if let Err(response) = download_blob(&state.blobs, storage, response).await {
                        return Ok(BlobFetch::Gateway(response));
                    }
                    store::record_blob_membership(&state.meta, &member.name, repo, digest)?;
                    return Ok(BlobFetch::Stored);
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
    Stored,
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
pub async fn download_blob(blobs: &BlobStore, storage: &Digest, response: reqwest::Response) -> Result<(), Response> {
    let stream = response.bytes_stream().map_err(|err| err.to_string());
    ingest_blob(blobs, storage, stream)
        .await
        .map_err(download_error_response)
}

/// Drain a byte stream into a staged blob and commit it under `storage`. Takes the transfer error
/// pre-stringified so this stays one instantiation a test can drive with a plain-string failure.
async fn ingest_blob(
    blobs: &BlobStore,
    storage: &Digest,
    stream: impl Stream<Item = Result<bytes::Bytes, String>> + Send,
) -> Result<(), DownloadError> {
    let mut pending = blobs.begin()?;
    let mut stream = std::pin::pin!(stream);
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(DownloadError::Stream)?;
        pending.write(&chunk)?;
    }
    blobs.commit(pending, storage)?;
    Ok(())
}

/// Map a failed ingest to a client response: a digest mismatch is the client's fault, the rest ours.
fn download_error_response(err: DownloadError) -> Response {
    match err {
        DownloadError::Blob(BlobError::DigestMismatch { expected, actual }) => error_response(
            ErrorCode::DigestInvalid,
            &format!("blob digest mismatch: expected {expected}, got {actual}"),
        ),
        DownloadError::Blob(err) => gateway_error(&format!("blob store error: {err}")),
        DownloadError::Stream(err) => gateway_error(&format!("blob body read failed: {err}")),
    }
}

pub(super) fn commit_blob(
    state: &ServingState,
    pending: PendingBlob,
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
    match state.blobs.commit(pending, &storage) {
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

/// Stream a stored blob, honoring a single-range request with `206`/`Content-Range`.
async fn serve_stored_blob(
    blobs: &BlobStore,
    storage: &Digest,
    digest: &str,
    head: bool,
    range: Option<&str>,
) -> Result<Response, ServeError> {
    let path = blobs.path_for(storage);
    let file = tokio::fs::File::open(&path).await?;
    let size = file.metadata().await?.len();
    let common = [
        (header::CONTENT_TYPE, HeaderValue::from_static(OCTET_STREAM)),
        (header::ACCEPT_RANGES, HeaderValue::from_static("bytes")),
        (
            DOCKER_CONTENT_DIGEST,
            HeaderValue::from_str(digest).unwrap_or(HeaderValue::from_static("")),
        ),
    ];
    // A range in a unit we do not speak, or a multi-range we do not serve as multipart, is ignored and
    // the whole blob served, as is any range we cannot parse.
    let spec = range
        .filter(|value| !value.contains(','))
        .map_or(RangeSpec::Ignore, |value| parse_range(value, size));
    let (start, end) = match spec {
        RangeSpec::Ignore => {
            let mut builder = Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_LENGTH, size);
            for (name, value) in common {
                builder = builder.header(name, value);
            }
            let body = if head {
                Body::empty()
            } else {
                peryx_driver::body::pipelined_file(file.into_std().await, 0, size)
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
    if head {
        return Ok(builder.body(Body::empty()).expect("range head response builds"));
    }
    Ok(builder
        .body(peryx_driver::body::pipelined_file(file.into_std().await, start, length))
        .expect("range response builds from validated header parts"))
}

/// A blob `HEAD` response: the size and digest headers a client needs to decide whether to pull, with
/// no body.
fn blob_head_response(digest: &str, size: u64, range: Option<&str>) -> Response {
    // A `HEAD` answers a `Range` the way the matching `GET` would. Ignoring it here while honouring
    // it for a cached blob made one request give two answers depending on what the store happened to
    // hold, which is the one thing a client checking a layer must not see.
    let spec = range
        .filter(|value| !value.contains(','))
        .map_or(RangeSpec::Ignore, |value| parse_range(value, size));
    let (status, length, content_range) = match spec {
        RangeSpec::Ignore => (StatusCode::OK, size, None),
        RangeSpec::Unsatisfiable => return unsatisfiable_range(size),
        RangeSpec::Satisfiable(start, end) => (
            StatusCode::PARTIAL_CONTENT,
            end - start + 1,
            Some(format!("bytes {start}-{end}/{size}")),
        ),
    };
    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, OCTET_STREAM)
        .header(header::CONTENT_LENGTH, length)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(
            DOCKER_CONTENT_DIGEST,
            HeaderValue::from_str(digest).unwrap_or(HeaderValue::from_static("")),
        );
    if let Some(content_range) = content_range {
        builder = builder.header(header::CONTENT_RANGE, content_range);
    }
    builder
        .body(Body::empty())
        .expect("blob head response builds from validated parts")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_download_error_maps_mismatch_to_client_and_the_rest_to_gateway() {
        let mismatch = DownloadError::Blob(BlobError::DigestMismatch {
            expected: "a".to_owned(),
            actual: "b".to_owned(),
        });
        assert_eq!(download_error_response(mismatch).status(), StatusCode::BAD_REQUEST);
        let io = DownloadError::Blob(BlobError::Io(std::io::Error::other("disk")));
        assert_eq!(download_error_response(io).status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            download_error_response(DownloadError::Stream("reset".to_owned())).status(),
            StatusCode::BAD_GATEWAY
        );
    }

    #[test]
    fn test_blob_fault_is_a_transport_error() {
        assert!(matches!(
            blob_fault(BlobError::NotFound("x".to_owned())),
            ServeError::Transport(_)
        ));
    }

    #[tokio::test]
    async fn test_ingest_blob_reports_a_stream_error() {
        let dir = tempfile::tempdir().unwrap();
        let blobs = BlobStore::new(dir.path().join("blobs"));
        let storage = Digest::of(b"x");
        let stream = futures_util::stream::iter(vec![Err("boom".to_owned())]);
        let err = ingest_blob(&blobs, &storage, stream).await.unwrap_err();
        assert!(matches!(err, DownloadError::Stream(message) if message == "boom"));
    }
}
