//! Blob serving: local and proxied reads, HEAD, ingest and delete.
//!
//! Global blob deduplication requires repository-scoped links before reads.

mod contents;

use contents::{layer_contents_response, layer_query_member};
use peryx_driver::conditional::applicable_range;
use peryx_driver::range::unsatisfiable_range;
use peryx_ha::fill_from_remote_placement;

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
use peryx_storage::blob::{
    BlobError, BlobErrorKind, BlobMetadata, BlobStorage, BlobWrite, Digest, RangeRequest, parse_range,
};
use peryx_storage::meta::OperationResult;
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
        if digest_decision(state, digest)? == DigestDecision::Revoked {
            return Ok(error_response(ErrorCode::BlobUnknown, "blob unknown"));
        }
        // A blob is content-addressed, so its digest is the strong validator for its bytes.
        let etag = format!("\"{digest}\"");
        let asked = BlobRequest {
            // A `HEAD` transfers no body, so a `Range` (and the `If-Range` that guards it) never
            // applies to it (RFC 9110 s14.2); only a `GET` resolves one.
            range: if head { None } else { applicable_range(headers, &etag) },
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
            let expected = response
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(0);
            let metrics = state.metrics.clone();
            let route = index.route.clone();
            let project = repo.to_owned();
            let filename = digest.to_owned();
            let body = std::mem::replace(response.body_mut(), Body::empty());
            *response.body_mut() = peryx_driver::body::on_body_complete(body, expected, move |bytes| {
                metrics.record(Event::Download {
                    route,
                    project,
                    filename,
                    // OCI layers are content-addressed with no version, and a stored serve has no cheap
                    // per-digest routed-upstream lookup, so both daily-usage labels stay empty here.
                    version: None,
                    source: None,
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
        // A blob this repository already authorizes but whose bytes are not local yet can come from a
        // peer that holds a verified placement, before falling back to an upstream member.
        if self.blob_authorized(state, index, repo, digest)?
            && let Some(metadata) = fill_remote(state, storage).await
        {
            state.cache.forget_flight(&gate_key);
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
        if digest_decision(state, digest)? == DigestDecision::Revoked {
            return Ok(error_response(ErrorCode::BlobUnknown, "blob unknown"));
        }
        match self.ensure_blob(state, index, repo, digest, &storage).await? {
            BlobFetch::Stored(_) => {}
            BlobFetch::Absent => return Ok(error_response(ErrorCode::BlobUnknown, "blob unknown")),
            BlobFetch::Gateway(response) => return Ok(response),
        }
        let lease = state.blobs.materialize(&storage).await.map_err(blob_fault)?;
        let selected = match layer_query_member(query) {
            Ok(selected) => selected,
            Err(response) => return Ok(response),
        };
        let task = tokio::task::spawn_blocking(move || layer_contents_response(lease.path(), selected));
        Ok(join_layer_contents(task).await)
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
            let deleted = crate::quota::release_blob_membership(&state.meta, &index.name, &repo, digest)?;
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

/// Fill the local content store from a verified remote placement, returning the stored metadata when a
/// peer served the blob. A single-node registry has no read-through installed, so this is a no-op there.
async fn fill_remote(state: &ServingState, storage: &Digest) -> Option<BlobMetadata> {
    fill_from_remote_placement(state.read_through(), &state.meta, &state.blobs, storage).await
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

/// Await the blocking layer-inspection task, turning a worker-thread panic into a structured gateway
/// error rather than letting the join failure abort the whole request.
async fn join_layer_contents(task: tokio::task::JoinHandle<Response>) -> Response {
    match task.await {
        Ok(response) => response,
        Err(err) => gateway_error(&format!("layer inspection failed: {err}")),
    }
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

/// A blob's placement is content-addressed and independent of any repository home, but recording that a
/// repository serves the digest is a metadata write under the repository's authority. An upload snapshots
/// the repository's committed epoch, then re-admits it before it records membership: an upload whose
/// authority did not move commits under the epoch it leased, while one whose home transferred while the
/// bytes arrived leased a superseded epoch and is turned away before it changes placement.
///
/// Snapshot the repository's committed authority epoch to re-admit before a blob upload records membership.
pub(super) async fn upload_epoch(state: &ServingState, repo: &str) -> u64 {
    state.committed_authority_epoch(repo).await
}

/// Whether an upload leased at `fence` may still record membership under `repo`'s committed epoch. A
/// process with no ownership group, or a repository no group has homed (`fence` of `0`), holds no epoch
/// and is never fenced.
pub(super) async fn epoch_admits(state: &ServingState, repo: &str, fence: u64) -> bool {
    fence == 0 || state.admit_authority_epoch(repo, fence).await
}

/// The retry response for an upload whose repository authority advanced past the epoch it leased. It is a
/// `503` a client retries, and it names no leader, datacenter, or membership, so it leaks no topology.
pub(super) fn authority_moved() -> Response {
    error_response(
        ErrorCode::Unavailable,
        "the repository authority moved while the upload was in flight; retry the upload",
    )
}

/// Release a still-open quota reservation, a no-op when the push was unmetered or the digest was already a
/// member. Every abandoned finalize - a rejected epoch, a commit fault, a superseded authority - returns
/// the momentary bytes it reserved so a fenced or failed upload leaves no phantom accounting behind.
pub(super) fn release_reservation(
    state: &ServingState,
    reservation: Option<peryx_storage::meta::QuotaReservationRecord>,
) -> Result<(), ServeError> {
    if let Some(record) = reservation {
        state.meta.release_quota_reservation(record.id)?;
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "a monolithic upload commits the staged blob, its quota reservation, and its outbox entry together"
)]
pub(super) async fn commit_blob(
    state: &ServingState,
    pending: BlobWrite,
    index: &Index,
    repo: &str,
    name: &str,
    digest: &str,
    bytes: u64,
    journal: bool,
) -> Result<Response, ServeError> {
    let Some(storage) = store::blob_digest(digest) else {
        return Ok(error_response(
            ErrorCode::DigestInvalid,
            "only sha256 blob digests are supported",
        ));
    };
    let fence = upload_epoch(state, repo).await;
    // A digest this repository already serves is accounted; re-pushing it must not reserve again.
    let reservation = if store::blob_is_member(&state.meta, &index.name, repo, digest)? {
        None
    } else {
        match crate::quota::admit_push(state, index, repo, None, digest, bytes)? {
            crate::quota::Admission::Rejected(response) => {
                pending.abort().await.map_err(blob_fault)?;
                return Ok(response);
            }
            crate::quota::Admission::Unmetered => None,
            crate::quota::Admission::Reserved(record) => Some(record),
        }
    };
    if !epoch_admits(state, repo, fence).await {
        release_reservation(state, reservation)?;
        pending.abort().await.map_err(blob_fault)?;
        return Ok(authority_moved());
    }
    let operation = blob_operation(&index.name, repo, digest);
    state.claim_admitted_write(&operation);
    match pending.commit(&storage).await {
        Ok(_receipt) => {
            crate::quota::commit_blob_membership(&state.meta, &index.name, repo, digest, reservation, None, journal)?;
            state.record_home_placement(storage.as_str(), bytes, fence);
            state.finalize_admitted_write(&operation, OperationResult::Published, b"");
            state.record_operation_trace(peryx_driver::state::OperationKind::Publish, fence);
            Ok(blob_created(name, digest))
        }
        Err(err) => {
            release_reservation(state, reservation)?;
            state.finalize_admitted_write(&operation, OperationResult::Failed, b"");
            Ok(download_error_response(DownloadError::Blob(err)))
        }
    }
}

/// The operation id an admitted blob write records under, stable across a client's retries: a re-push of
/// the same digest to the same repository resolves to one id, so its outcome dedups to a single ledger
/// record. The id keys only the recording; the commit itself always runs, so a re-push stays retrievable.
fn blob_operation(index: &str, repo: &str, digest: &str) -> String {
    format!("oci:{index}:{repo}:{digest}")
}

/// Publish a session's durable stage under `digest` and record its membership, the resumable
/// counterpart to [`commit_blob`].
///
/// On success the session's durable record is closed. A rejected quota drops the stage and record. A
/// commit fault - a digest mismatch, most likely - keeps both, so the client can retry the finalize
/// with the right digest rather than re-upload every byte.
#[allow(
    clippy::too_many_arguments,
    reason = "a resumable upload commits the staged blob, its quota reservation, and its outbox entry together"
)]
pub(super) async fn commit_staged_upload(
    state: &ServingState,
    session: &str,
    index: &Index,
    repo: &str,
    name: &str,
    digest: &str,
    storage: Digest,
    bytes: u64,
    journal: bool,
) -> Result<Response, ServeError> {
    let fence = upload_epoch(state, repo).await;
    let reservation = if store::blob_is_member(&state.meta, &index.name, repo, digest)? {
        None
    } else {
        match crate::quota::admit_push(state, index, repo, None, digest, bytes)? {
            crate::quota::Admission::Rejected(response) => {
                state.blobs.discard_upload(session).await.map_err(blob_fault)?;
                state.meta.remove_upload(session)?;
                return Ok(response);
            }
            crate::quota::Admission::Unmetered => None,
            crate::quota::Admission::Reserved(record) => Some(record),
        }
    };
    if !epoch_admits(state, repo, fence).await {
        // Keep the durable stage and its session so status stays usable and a retry, once the transfer
        // has settled, finalizes without re-uploading a byte; only the momentary reservation is released.
        release_reservation(state, reservation)?;
        return Ok(authority_moved());
    }
    let operation = blob_operation(&index.name, repo, digest);
    state.claim_admitted_write(&operation);
    match state.blobs.finish_upload(session, &storage).await {
        Ok(()) => {
            let committed = crate::quota::commit_blob_membership(
                &state.meta,
                &index.name,
                repo,
                digest,
                reservation,
                Some(session),
                journal,
            );
            committed?;
            state.record_home_placement(storage.as_str(), bytes, fence);
            state.finalize_admitted_write(&operation, OperationResult::Published, b"");
            state.record_operation_trace(peryx_driver::state::OperationKind::Publish, fence);
            Ok(blob_created(name, digest))
        }
        Err(err) => {
            release_reservation(state, reservation)?;
            state.finalize_admitted_write(&operation, OperationResult::Failed, b"");
            Ok(download_error_response(DownloadError::Blob(err)))
        }
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
    let range = match parse_range(asked.range, size) {
        RangeRequest::Whole => {
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
        RangeRequest::Unsatisfiable => return Ok(unsatisfiable_range(size)),
        RangeRequest::Partial(range) => range,
    };
    let length = range.end - range.start;
    let mut builder = Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(header::CONTENT_LENGTH, length)
        .header(
            header::CONTENT_RANGE,
            format!("bytes {}-{}/{size}", range.start, range.end - 1),
        );
    for (name, value) in common {
        builder = builder.header(name, value);
    }
    Ok(builder
        .body(peryx_driver::body::blob_read(
            blobs.open(storage, Some(range)).await.map_err(blob_fault)?,
        ))
        .expect("range response builds from validated header parts"))
}

/// A blob `HEAD` response: the size and digest headers a client needs to decide whether to pull, with
/// no body. A `HEAD` transfers no content, so a `Range` never applies (RFC 9110 s14.2) and an existing
/// blob always answers `200` with its full representation size (OCI distribution spec).
fn blob_head_response(digest: &str, size: Option<u64>, asked: &BlobRequest<'_>) -> Response {
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, OCTET_STREAM)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, header_value(asked.etag))
        .header(DOCKER_CONTENT_DIGEST, header_value(digest));
    if let Some(size) = size {
        builder = builder.header(header::CONTENT_LENGTH, size);
    }
    let body = size.map_or_else(
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
    async fn test_join_layer_contents_returns_the_task_response() {
        let task = tokio::task::spawn_blocking(|| error_response(ErrorCode::BlobUnknown, "blob unknown"));
        assert_eq!(join_layer_contents(task).await.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_join_layer_contents_maps_a_panic_to_a_gateway_error() {
        let task = tokio::task::spawn_blocking(|| -> Response { panic!("layer worker blew up") });
        assert_eq!(join_layer_contents(task).await.status(), StatusCode::BAD_GATEWAY);
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
