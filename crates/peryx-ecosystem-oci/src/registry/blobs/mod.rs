//! Global blob deduplication requires repository-scoped links before reads.

mod contents;

use contents::{layer_contents_response, layer_query_member};
use peryx_driver::conditional::applicable_range;
use peryx_driver::range::unsatisfiable_range;

use super::uploads::created;
use super::*;
use crate::error::{ErrorCode, error_response, gateway_error};
use crate::registry::acknowledge::{BlobAck, acknowledge_blob};
use crate::registry::admission;
use crate::registry::authority::{EpochCommit, claim_repository_home, commit_epoch};
use crate::store::{self};
use crate::upstream::UpstreamError;
use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use futures_util::{Stream, TryStreamExt as _};
use peryx_driver::ServingState;
use peryx_events::metrics::Observation;
use peryx_index::Index;
use peryx_policy::PolicyAction;
use peryx_storage::blob::{
    BlobError, BlobErrorKind, BlobMetadata, BlobStorage, BlobWrite, Digest, RangeRequest, parse_range,
};
use peryx_storage::meta::{MetaStore, OperationResult};
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
        let members = policy_serving_members(state, index, repo);
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
        let etag = digest_etag(digest);
        let asked = BlobRequest {
            // A `HEAD` transfers no body, so a `Range` (and the `If-Range` that guards it) never
            // applies to it (RFC 9110 s14.2); only a `GET` resolves one.
            range: if head { None } else { applicable_range(headers, &etag) },
            unchanged: if_none_match_holds(headers, &etag),
            etag: &etag,
            head,
        };
        if head {
            return self.head_blob(state, &members, repo, digest, &storage, &asked).await;
        }
        let mut response = match self.ensure_blob(state, &members, repo, digest, &storage).await? {
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
                metrics.record(Observation::Read {
                    repository: route,
                    resource: project,
                    artifact: filename,
                    // OCI layers are content-addressed with no group: version, and a stored serve has no cheap
                    // per-digest routed-upstream lookup, so both daily-usage labels stay empty here.
                    group: None,
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
        members: &[&Index],
        repo: &str,
        digest: &str,
        storage: &Digest,
        asked: &BlobRequest<'_>,
    ) -> Result<Response, ServeError> {
        if let Some(metadata) = state.blobs.head(storage).await.map_err(ServeError::from)?
            && self.blob_authorized_in(state, members, repo, digest)?
        {
            return serve_stored_blob(&state.blobs, storage, digest, metadata.bytes, asked).await;
        }
        for member in members {
            let Some(client) = member.proxy_client() else {
                continue;
            };
            match self
                .upstream
                .blob_head(
                    client,
                    &self.upstream_repo(&member.name, client, repo),
                    digest,
                    &self.token_realms(&member.name),
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

    async fn ensure_blob(
        &self,
        state: &ServingState,
        members: &[&Index],
        repo: &str,
        digest: &str,
        storage: &Digest,
    ) -> Result<BlobFetch, ServeError> {
        if let Some(metadata) = state.blobs.head(storage).await.map_err(ServeError::from)?
            && self.blob_authorized_in(state, members, repo, digest)?
        {
            return Ok(BlobFetch::Stored(metadata));
        }
        let gate_key = format!("oci\0blob\0{digest}");
        let gate = flight_gate(state, &gate_key);
        let _guard = gate.lock().await;
        if let Some(metadata) = state.blobs.head(storage).await.map_err(ServeError::from)?
            && self.blob_authorized_in(state, members, repo, digest)?
        {
            return Ok(BlobFetch::Stored(metadata));
        }
        // A blob this repository already authorizes but whose bytes are not local yet can come from a
        // peer that holds a verified placement, before falling back to an upstream member.
        if self.blob_authorized_in(state, members, repo, digest)?
            && let Some(metadata) = fill_remote(state, storage).await
        {
            state.cache.forget_flight(&gate_key);
            return Ok(BlobFetch::Stored(metadata));
        }
        let fetched = self.fetch_blob(state, members, repo, digest, storage).await;
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
        let members = policy_serving_members(state, index, repo);
        let Some(storage) = store::blob_digest(digest) else {
            return Ok(error_response(
                ErrorCode::DigestInvalid,
                "only sha256 blob digests are supported",
            ));
        };
        if digest_decision(state, digest)? == DigestDecision::Revoked {
            return Ok(error_response(ErrorCode::BlobUnknown, "blob unknown"));
        }
        match self.ensure_blob(state, &members, repo, digest, &storage).await? {
            BlobFetch::Stored(_) => {}
            BlobFetch::Absent => return Ok(error_response(ErrorCode::BlobUnknown, "blob unknown")),
            BlobFetch::Gateway(response) => return Ok(response),
        }
        let lease = state.blobs.materialize(&storage).await.map_err(ServeError::from)?;
        let selected = match layer_query_member(query) {
            Ok(selected) => selected,
            Err(rejection) => return Ok(rejection.into_response()),
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
        let stored = state.blobs.head(storage).await.map_err(ServeError::from)?;
        for member in members {
            let Some(client) = member.proxy_client() else {
                continue;
            };
            if let Some(metadata) = stored {
                match self
                    .upstream
                    .blob_head(
                        client,
                        &self.upstream_repo(&member.name, client, repo),
                        digest,
                        &self.token_realms(&member.name),
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
                    client,
                    &self.upstream_repo(&member.name, client, repo),
                    digest,
                    &self.token_realms(&member.name),
                )
                .await
            {
                Ok(response) => {
                    let bytes = match download_blob(&state.meta, &state.blobs, storage, response).await {
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

    pub(super) async fn delete_blob(
        &self,
        state: &Arc<ServingState>,
        headers: &HeaderMap,
        name: &str,
        digest: &str,
    ) -> Result<Response, ServeError> {
        let (index, repo, identity) = match resolve_writable(state, name, headers, Action::Delete) {
            Ok(target) => target,
            Err(rejection) => return Ok(rejection.into_response()),
        };
        if store::blob_digest(digest).is_none() {
            return Ok(error_response(
                ErrorCode::DigestInvalid,
                "only sha256 blob digests are supported",
            ));
        }
        let fence = match claim_repository_home(state, &repo).await {
            Ok(fence) => fence,
            Err(response) => return Ok(response),
        };
        let membership = store::blob_membership_key(&index.name, &repo, digest);
        let webhook = prepare_webhook(
            state,
            &Requester {
                headers,
                identity: &identity,
            },
            crate::webhook::BLOB_DELETE,
            index,
            &repo,
            None,
            Some(digest),
        );
        let mutation = commit_epoch(state, &repo, fence, |lease| {
            lease.guard()?;
            self.blob_memberships.write().remove(&membership);
            crate::quota::release_blob_membership(&state.meta, &index.name, &repo, digest, webhook, self.journal_outbox)
        })
        .await?;
        let deleted = match mutation {
            EpochCommit::Committed(deleted) => deleted,
            EpochCommit::Fenced => return Ok(authority_moved()),
        };
        if !deleted {
            return Ok(error_response(ErrorCode::BlobUnknown, "blob unknown"));
        }
        peryx_events::webhook::notify(state.as_ref());
        Ok(accepted())
    }

    pub(super) fn blob_authorized(
        &self,
        state: &ServingState,
        index: &Index,
        repo: &str,
        digest: &str,
    ) -> Result<bool, ServeError> {
        self.blob_authorized_in(state, &serving_members(state, index), repo, digest)
    }

    fn blob_authorized_in(
        &self,
        state: &ServingState,
        members: &[&Index],
        repo: &str,
        digest: &str,
    ) -> Result<bool, ServeError> {
        for member in members {
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
    match state.ensure_blob_local(storage).await {
        Ok(metadata) => metadata,
        Err(error) => {
            tracing::warn!(digest = storage.as_str(), %error, "remote placement read-through failed");
            None
        }
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

/// Await the blocking layer-inspection task, turning a worker-thread panic into a structured gateway
/// error rather than letting the join failure abort the whole request.
async fn join_layer_contents(task: tokio::task::JoinHandle<Response>) -> Response {
    match task.await {
        Ok(response) => response,
        Err(err) => gateway_error(&format!("layer inspection failed: {err}")),
    }
}

pub async fn download_blob(
    meta: &MetaStore,
    blobs: &BlobStorage,
    storage: &Digest,
    response: reqwest::Response,
) -> Result<u64, DownloadError> {
    let stream = response.bytes_stream().map_err(|err| err.to_string());
    let bytes = ingest_blob(blobs, storage, Box::pin(stream)).await?;
    // Both callers reach here by pulling a blob this node did not host, so the projection records the
    // one thing a later read wants to know without probing the content store: the bytes are here and
    // an upstream can resupply them. Taking the store here is what stops a third caller forgetting.
    if let Err(error) = store::record_content_placement(meta, storage.as_str(), store::OciArtifactOrigin::Mirrored, true) {
        tracing::warn!(digest = storage.as_str(), %error, "recording the mirrored blob placement failed");
    }
    Ok(bytes)
}

/// Drain a byte stream into a staged blob and commit it under `storage`. Takes the transfer error
/// pre-stringified so this stays one instantiation a test can drive with a plain-string failure.
async fn ingest_blob(
    blobs: &BlobStorage,
    storage: &Digest,
    mut stream: std::pin::Pin<Box<dyn Stream<Item = Result<bytes::Bytes, String>> + Send>>,
) -> Result<u64, DownloadError> {
    let mut pending = blobs.begin().await?;
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
/// repository serves the digest is a metadata write under the repository's authority.
///
/// Snapshot the repository's committed authority epoch before a blob upload records membership.
pub(super) async fn upload_epoch(state: &ServingState, repo: &str) -> Result<u64, Response> {
    claim_repository_home(state, repo).await
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

/// A blob whose bytes are committed and digest-verified, offered for durable ingress admission before
/// any repository membership records it.
struct IngressUpload<'a> {
    index: &'a str,
    repo: &'a str,
    digest: &'a str,
    bytes: u64,
    operation: &'a str,
    /// The resumable session the publication closes; `None` for a monolithic push.
    session: Option<&'a str>,
    reservation: Option<&'a peryx_storage::meta::QuotaReservationRecord>,
}

/// Retain `upload` for home finalization. Called only once its bytes are durable and its digest
/// verified, so the record a crash can leave behind always names content that is already stored.
fn stage_ingress_intent(state: &ServingState, upload: &IngressUpload<'_>) -> Result<admission::Admission, ServeError> {
    admission::admit(
        &state.meta,
        admission::STAGING_LIMITS,
        &admission::AdmissionRequest {
            index: upload.index,
            repo: upload.repo,
            digest: upload.digest,
            size: upload.bytes,
            operation: upload.operation,
            session: upload.session,
            reservation: upload.reservation,
            ingress_dc: &admission::ingress_dc(state.availability_topology()),
        },
        (state.clock)(),
    )
}

/// Give back everything a shed push must not keep - its quota reservation and its claimed operation -
/// and answer the backoff the client retries on. The bytes stay: they are content-addressed, so the
/// retry commits the same object rather than transferring it again, and content cleanup reclaims them
/// if it never comes.
fn shed_push(
    state: &ServingState,
    reservation: Option<peryx_storage::meta::QuotaReservationRecord>,
    operation: &str,
    response: Response,
) -> Result<Response, ServeError> {
    release_reservation(state, reservation)?;
    state.finalize_admitted_write(operation, OperationResult::Failed, b"");
    Ok(response)
}

/// Settle the retained intent whose membership just committed, so the reaper can reclaim it and no
/// later sweep republishes a write that is already visible.
fn settle_ingress_intent(state: &ServingState, intent: &str) -> Result<(), ServeError> {
    state
        .meta
        .advance_intent(intent, peryx_storage::meta::IntentPhase::Admitted, (state.clock)())?;
    Ok(())
}

pub(super) struct BlobCommitContext<'a> {
    pub(super) state: &'a ServingState,
    pub(super) index: &'a Index,
    pub(super) repo: &'a str,
    pub(super) name: &'a str,
    pub(super) digest: &'a str,
    pub(super) bytes: u64,
    pub(super) journal: crate::outbox::Outbox,
}

pub(super) async fn commit_blob(context: BlobCommitContext<'_>, pending: BlobWrite) -> Result<Response, ServeError> {
    let BlobCommitContext {
        state,
        index,
        repo,
        name,
        digest,
        bytes,
        journal,
    } = context;
    let Some(storage) = store::blob_digest(digest) else {
        return Ok(error_response(
            ErrorCode::DigestInvalid,
            "only sha256 blob digests are supported",
        ));
    };
    let fence = match upload_epoch(state, repo).await {
        Ok(fence) => fence,
        Err(response) => {
            pending.abort().await.map_err(ServeError::from)?;
            return Ok(response);
        }
    };
    // A digest this repository already serves is accounted; re-pushing it must not reserve again.
    let reservation = if store::blob_is_member(&state.meta, &index.name, repo, digest)? {
        None
    } else {
        match crate::quota::admit_push(state, index, repo, None, digest, bytes)? {
            crate::quota::Admission::Rejected(response) => {
                pending.abort().await.map_err(ServeError::from)?;
                return Ok(response);
            }
            crate::quota::Admission::Unmetered => None,
            crate::quota::Admission::Reserved(record) => Some(record),
        }
    };
    let operation = blob_operation(&index.name, repo, digest);
    state.claim_admitted_write(&operation);
    match pending.commit(&storage).await {
        Ok(receipt) => {
            let intent = match stage_ingress_intent(
                state,
                &IngressUpload {
                    index: &index.name,
                    repo,
                    digest,
                    bytes: receipt.size,
                    operation: &operation,
                    session: None,
                    reservation: reservation.as_ref(),
                },
            )? {
                admission::Admission::Staged(key) => key,
                admission::Admission::Shed(response) => return shed_push(state, reservation, &operation, *response),
            };
            let mutation = commit_epoch(state, repo, fence, |lease| {
                lease.guard()?;
                let commit = crate::quota::commit_blob_membership(
                    &state.meta,
                    &index.name,
                    repo,
                    digest,
                    reservation.clone(),
                    None,
                    journal,
                )?;
                lease.guard()?;
                state.record_home_placement(storage.as_str(), bytes, fence);
                Ok(commit)
            })
            .await?;
            // A fenced membership leaves the retained intent, its reservation, and its operation
            // exactly as they are: the bytes are durable here and the write is still finalizable, so
            // the home publishes it rather than the client losing it to a mid-flight transfer.
            let EpochCommit::Committed(commit) = mutation else {
                return Ok(authority_moved());
            };
            settle_ingress_intent(state, &intent)?;
            publish_acknowledged(
                state,
                &operation,
                BlobAck {
                    repo,
                    digest: &receipt.digest,
                    bytes: receipt.size,
                    commit,
                    evidence: receipt.evidence,
                },
                || blob_created(name, digest),
            )
            .await
        }
        Err(err) => {
            release_reservation(state, reservation)?;
            state.finalize_admitted_write(&operation, OperationResult::Failed, b"");
            Ok(download_error_response(DownloadError::Blob(err)))
        }
    }
}

/// Publish an admitted write and answer its success response, but only once the configured
/// acknowledgement policy proves the write durable.
///
/// A policy the deadline leaves unproven answers the retry response and leaves the operation pending,
/// so the client's identical retry re-drives the same content-addressed commit and membership upsert
/// and finishes the same operation rather than starting a second one.
pub(super) async fn publish_acknowledged(
    state: &ServingState,
    operation: &str,
    ack: BlobAck<'_>,
    success: impl FnOnce() -> Response,
) -> Result<Response, ServeError> {
    match acknowledge_blob(state, ack).await {
        Ok(()) => {
            state.finalize_admitted_write(operation, OperationResult::Published, b"");
            Ok(success())
        }
        Err(response) => Ok(response),
    }
}

/// The operation id an admitted blob write records under, stable across a client's retries: a re-push of
/// the same digest to the same repository resolves to one id, so its outcome dedups to a single ledger
/// record. A mount shares that id, since it makes the same repository serve the same digest. The id keys
/// only the recording; the commit itself always runs, so a re-push stays retrievable.
pub(super) fn blob_operation(index: &str, repo: &str, digest: &str) -> String {
    format!("oci:{index}:{repo}:{digest}")
}

/// Publish a session's durable stage under `digest` and record its membership, the resumable
/// counterpart to [`commit_blob`].
///
/// On success the session's durable record is closed. A rejected quota drops the stage and record. A
/// commit fault - a digest mismatch, most likely - keeps both, so the client can retry the finalize
/// with the right digest rather than re-upload every byte.
pub(super) async fn commit_staged_upload(
    context: BlobCommitContext<'_>,
    session: &str,
    storage: Digest,
) -> Result<Response, ServeError> {
    let BlobCommitContext {
        state,
        index,
        repo,
        name,
        digest,
        bytes,
        journal,
    } = context;
    let fence = match upload_epoch(state, repo).await {
        Ok(fence) => fence,
        Err(response) => return Ok(response),
    };
    let reservation = if store::blob_is_member(&state.meta, &index.name, repo, digest)? {
        None
    } else {
        match crate::quota::admit_push(state, index, repo, None, digest, bytes)? {
            crate::quota::Admission::Rejected(response) => {
                state.blobs.discard_upload(session).await.map_err(ServeError::from)?;
                state.meta.remove_upload(session)?;
                return Ok(response);
            }
            crate::quota::Admission::Unmetered => None,
            crate::quota::Admission::Reserved(record) => Some(record),
        }
    };
    let operation = blob_operation(&index.name, repo, digest);
    state.claim_admitted_write(&operation);
    match state.blobs.finish_upload(session, &storage).await {
        Ok(receipt) => {
            let intent = match stage_ingress_intent(
                state,
                &IngressUpload {
                    index: &index.name,
                    repo,
                    digest,
                    bytes: receipt.size,
                    operation: &operation,
                    session: Some(session),
                    reservation: reservation.as_ref(),
                },
            )? {
                admission::Admission::Staged(key) => key,
                admission::Admission::Shed(response) => return shed_push(state, reservation, &operation, *response),
            };
            let mutation = commit_epoch(state, repo, fence, |lease| {
                lease.guard()?;
                let commit = crate::quota::commit_blob_membership(
                    &state.meta,
                    &index.name,
                    repo,
                    digest,
                    reservation.clone(),
                    Some(session),
                    journal,
                )?;
                lease.guard()?;
                state.record_home_placement(storage.as_str(), bytes, fence);
                Ok(commit)
            })
            .await?;
            // A fenced membership leaves the retained intent, its reservation, and its operation
            // exactly as they are: the bytes are durable here and the write is still finalizable, so
            // the home publishes it rather than the client losing it to a mid-flight transfer.
            let EpochCommit::Committed(commit) = mutation else {
                return Ok(authority_moved());
            };
            settle_ingress_intent(state, &intent)?;
            publish_acknowledged(
                state,
                &operation,
                BlobAck {
                    repo,
                    digest: &receipt.digest,
                    bytes: receipt.size,
                    commit,
                    evidence: receipt.evidence,
                },
                || blob_created(name, digest),
            )
            .await
        }
        Err(err) => {
            release_reservation(state, reservation)?;
            state.finalize_admitted_write(&operation, OperationResult::Failed, b"");
            Ok(download_error_response(DownloadError::Blob(err)))
        }
    }
}

pub(super) fn blob_created(name: &str, digest: &str) -> Response {
    created(&format!("/v2/{name}/blobs/{digest}"), digest)
}

struct BlobRequest<'a> {
    /// The blob's entity tag, sent with every response so a client has a validator to condition on.
    etag: &'a str,
    /// The single range to serve, or `None` for the whole blob.
    range: Option<&'a str>,
    /// Whether an `If-None-Match` field already named this blob, so the client holds its bytes.
    unchanged: bool,
    head: bool,
}

async fn serve_stored_blob(
    blobs: &BlobStorage,
    storage: &Digest,
    digest: &str,
    size: u64,
    asked: &BlobRequest<'_>,
) -> Result<Response, ServeError> {
    // RFC 9110 s13.1.2 evaluates `If-None-Match` ahead of `Range`, so a client that already holds
    // these bytes gets the validators back rather than the body or a slice of it.
    if asked.unchanged {
        return Ok(blob_not_modified(digest, asked));
    }
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
                peryx_driver::body::blob_read(blobs.open(storage, None).await.map_err(ServeError::from)?)
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
            blobs.open(storage, Some(range)).await.map_err(ServeError::from)?,
        ))
        .expect("range response builds from validated header parts"))
}

/// A blob `HEAD` response: the size and digest headers a client needs to decide whether to pull, with
/// no body. A `HEAD` transfers no content, so a `Range` never applies (RFC 9110 s14.2) and an existing
/// blob always answers `200` with its full representation size (OCI distribution spec).
fn blob_head_response(digest: &str, size: Option<u64>, asked: &BlobRequest<'_>) -> Response {
    if asked.unchanged {
        return blob_not_modified(digest, asked);
    }
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

/// The `304` for a blob the client already holds: the validators plus the range capability a `200`
/// would have carried, so its next conditional or partial pull has everything it needs.
fn blob_not_modified(digest: &str, asked: &BlobRequest<'_>) -> Response {
    not_modified(asked.etag, digest)
        .header(header::ACCEPT_RANGES, "bytes")
        .body(Body::empty())
        .expect("not-modified response builds from validated header parts")
}

fn header_value(value: &str) -> HeaderValue {
    HeaderValue::from_str(value).unwrap_or(HeaderValue::from_static(""))
}

#[cfg(test)]
#[path = "../../../tests/unit/registry/blobs/tests.rs"]
mod tests;
