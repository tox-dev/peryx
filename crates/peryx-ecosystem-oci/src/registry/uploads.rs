use super::blobs::{
    BlobCommitContext, authority_moved, blob_created, blob_operation, commit_blob, commit_staged_upload,
    publish_acknowledged, release_reservation, upload_epoch,
};
use super::*;
use crate::error::{ErrorCode, error_response};
use crate::registry::acknowledge::BlobAck;
use crate::registry::authority::{EpochCommit, commit_epoch};
use crate::store::{self};
use crate::upload_session::UploadRecord;
use axum::body::Body;
use axum::http::response::Builder;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use http_body::Body as _;
use peryx_driver::ServingState;
use peryx_storage::blob::Digest;
use peryx_storage::meta::OperationResult;

impl<S: BuildHasher + Default + Send + Sync + 'static> OciRegistryWithHasher<S> {
    /// Begin a blob upload: cross-repo mount when the blob is already stored, a monolithic write when
    /// the `POST` carries a `digest`, otherwise a session the client fills with `PATCH`/`PUT`.
    pub(super) async fn start_upload(
        &self,
        state: &ServingState,
        headers: &HeaderMap,
        query: &str,
        name: &str,
        body: Body,
    ) -> Result<Response, ServeError> {
        let (index, repo, _) = match resolve_uploadable(state, name, headers) {
            Ok(target) => target,
            Err(rejection) => return Ok(rejection.into_response()),
        };
        let journal = self.journal_outbox;
        let params = query_params(query);
        if let (Some(mount), Some(source)) = (params.get("mount"), params.get("from"))
            && let Some(storage) = store::blob_digest(mount)
        {
            if let Err(rejection) = auth::authorize_read(state, headers, source) {
                return Ok(rejection.into_response());
            }
            if let Some((source_index, source_repo)) = resolve(&state.indexes, source)
                && !policy_blocks(source_index, PolicyAction::Serve, source_repo)
                && let Some(metadata) = state.blobs.head(&storage).await.map_err(ServeError::from)?
                && self.blob_authorized(state, source_index, source_repo, mount)?
            {
                return mount_blob(
                    state,
                    MountRequest {
                        index,
                        repo: &repo,
                        name,
                        mount,
                        storage: &storage,
                        bytes: metadata.bytes,
                        journal,
                    },
                )
                .await;
            }
        }
        if let Some(digest) = params.get("digest") {
            let mut pending = state.blobs.begin().await.map_err(ServeError::from)?;
            let mut size = 0;
            if let Err(err) = append_body(&mut pending, &mut size, body, index, &repo).await {
                return err.into_response();
            }
            return commit_blob(
                BlobCommitContext {
                    state,
                    index,
                    repo: &repo,
                    name,
                    digest,
                    bytes: size,
                    journal,
                },
                pending,
            )
            .await;
        }
        let now = (state.clock)();
        let session = Self::random_session()?;
        // The session is durable from the first byte: a restart between chunks recovers it from the
        // store and resumes at the recorded offset rather than losing the upload. The stage is opened
        // empty now so a zero-byte upload finalized without any chunk still has bytes to verify.
        state.meta.begin_upload(&session, &index.name, name, now)?;
        if let Err(error) = state.blobs.stage_upload_chunk(&session, 0, b"").await {
            // A stage that failed part way may still hold a file, and the record is what the reclaim
            // sweep needs to find it, so the bytes go first and the record only once they are gone.
            state.blobs.discard_upload(&session).await.map_err(ServeError::from)?;
            state.meta.remove_upload(&session)?;
            return Err(error.into());
        }
        Ok(upload_accepted(name, &session, 0))
    }

    /// Cancel an open upload session (spec end-14): remove its staged bytes and answer `204`, or `404`
    /// when the id names no session this index opened.
    pub(super) async fn cancel_upload(
        &self,
        state: &ServingState,
        headers: &HeaderMap,
        name: &str,
        session: &str,
    ) -> Result<Response, ServeError> {
        let (index, _, _) = match resolve_writable(state, name, headers, Action::Write) {
            Ok(target) => target,
            Err(rejection) => return Ok(rejection.into_response()),
        };
        let Some(_record) = session_record(state, &index.name, name, session)? else {
            return Ok(error_response(ErrorCode::BlobUploadUnknown, "upload unknown"));
        };
        state.blobs.discard_upload(session).await.map_err(ServeError::from)?;
        state.meta.remove_upload(session)?;
        Ok(StatusCode::NO_CONTENT.into_response())
    }

    /// Report an open upload session's progress: `204` with the bytes received so far.
    pub(super) fn upload_status(
        state: &ServingState,
        headers: &HeaderMap,
        name: &str,
        session: &str,
    ) -> Result<Response, ServeError> {
        let (index, _, _) = match resolve_writable(state, name, headers, Action::Write) {
            Ok(target) => target,
            Err(rejection) => return Ok(rejection.into_response()),
        };
        let Some(record) = session_record(state, &index.name, name, session)? else {
            return Ok(error_response(ErrorCode::BlobUploadUnknown, "upload unknown"));
        };
        // A status read is activity, so it keeps the session alive against the idle TTL.
        state.meta.advance_upload(session, record.offset, (state.clock)())?;
        Ok(upload_status_response(name, session, record.offset))
    }

    pub(super) async fn patch_upload(
        &self,
        state: &ServingState,
        headers: &HeaderMap,
        name: &str,
        session: &str,
        body: Body,
    ) -> Result<Response, ServeError> {
        let (index, repo, _) = match resolve_uploadable(state, name, headers) {
            Ok(target) => target,
            Err(rejection) => return Ok(rejection.into_response()),
        };
        // Serialize this session's read-modify-write so a concurrent chunk cannot read the same offset
        // and interleave its bytes into the stage.
        let lock = self.session_gate.lock(session);
        let outcome = {
            let _guard = lock.lock_owned().await;
            patch_locked(state, index, &repo, name, session, headers, body).await
        };
        self.session_gate.release(session);
        outcome
    }

    pub(super) async fn finish_upload(
        &self,
        state: &ServingState,
        headers: &HeaderMap,
        query: &str,
        name: &str,
        session: &str,
        body: Body,
    ) -> Result<Response, ServeError> {
        let (index, repo, _) = match resolve_uploadable(state, name, headers) {
            Ok(target) => target,
            Err(rejection) => return Ok(rejection.into_response()),
        };
        let lock = self.session_gate.lock(session);
        let outcome = {
            let _guard = lock.lock_owned().await;
            finish_locked(FinishUpload {
                state,
                index,
                repo: &repo,
                name,
                session,
                query,
                headers,
                body,
                journal: self.journal_outbox,
            })
            .await
        };
        self.session_gate.release(session);
        outcome
    }
}

struct MountRequest<'a> {
    index: &'a Index,
    repo: &'a str,
    /// The repository path the client pushed to, which the created response echoes.
    name: &'a str,
    /// The mounted digest in its `sha256:` wire form.
    mount: &'a str,
    /// The same digest as the blob store addresses it.
    storage: &'a Digest,
    bytes: u64,
    journal: crate::outbox::Outbox,
}

/// Publish an already-stored blob into `repo` without transferring its bytes (spec end-11).
///
/// A mount is a terminal write like any push: it claims an operation, records membership under the
/// repository's authority, and answers `201` only once the configured policy proves the copy it
/// published durable. The bytes are already resident, so the evidence it presents is what the backend
/// proves about the object already at that address.
async fn mount_blob(state: &ServingState, request: MountRequest<'_>) -> Result<Response, ServeError> {
    let MountRequest {
        index,
        repo,
        name,
        mount,
        storage,
        bytes,
        journal,
    } = request;
    if let Some(response) = policy_size_denial(index, repo, bytes) {
        return Ok(response);
    }
    let fence = match upload_epoch(state, repo).await {
        Ok(fence) => fence,
        Err(response) => return Ok(response),
    };
    // A mount publishes an existing blob into this repository without a transfer, so it reserves the
    // mounted digest's bytes exactly as an upload of them would; a digest already served here is not
    // reserved again.
    let reservation = if store::blob_is_member(&state.meta, &index.name, repo, mount)? {
        None
    } else {
        match crate::quota::admit_push(state, index, repo, None, mount, bytes)? {
            crate::quota::Admission::Rejected(response) => return Ok(response),
            crate::quota::Admission::Unmetered => None,
            crate::quota::Admission::Reserved(record) => Some(record),
        }
    };
    let operation = blob_operation(&index.name, repo, mount);
    state.claim_admitted_write(&operation);
    let mutation = commit_epoch(state, repo, fence, |lease| {
        lease.guard()?;
        crate::quota::commit_blob_membership(
            &state.meta,
            &index.name,
            repo,
            mount,
            reservation.clone(),
            None,
            journal,
        )
    })
    .await?;
    let EpochCommit::Committed(commit) = mutation else {
        release_reservation(state, reservation)?;
        state.finalize_admitted_write(&operation, OperationResult::Failed, b"");
        return Ok(authority_moved());
    };
    publish_acknowledged(
        state,
        &operation,
        fence,
        BlobAck {
            repo,
            digest: storage,
            bytes,
            commit,
            evidence: state.blobs.resident_evidence(),
        },
        || blob_created(name, mount),
    )
    .await
}

async fn patch_locked(
    state: &ServingState,
    index: &Index,
    repo: &str,
    name: &str,
    session: &str,
    headers: &HeaderMap,
    body: Body,
) -> Result<Response, ServeError> {
    match append_session_chunk(state, index, repo, name, session, headers, body).await? {
        Ok(offset) => Ok(upload_accepted(name, session, offset)),
        Err(rejection) => Ok(rejection.into_response()),
    }
}

/// Append any trailing bytes under the session lock and commit under the closing digest. Run inside the
/// per-session guard so the append and the commit see a stage no other writer can touch.
///
/// The session and the chunk are validated first, so an unknown session answers 404 and an out-of-order
/// chunk answers 416 rather than being masked by digest validation. The digest is then checked before the
/// body is read: a closing `PUT` that omits it or names an algorithm the store cannot key on is rejected
/// with the stage, offset, and activity timestamp untouched, so a client that fixes the URL and resends
/// the same final chunk does not append those bytes twice.
struct FinishUpload<'a> {
    state: &'a ServingState,
    index: &'a Index,
    repo: &'a str,
    name: &'a str,
    session: &'a str,
    query: &'a str,
    headers: &'a HeaderMap,
    body: Body,
    journal: crate::outbox::Outbox,
}

async fn finish_locked(request: FinishUpload<'_>) -> Result<Response, ServeError> {
    let FinishUpload {
        state,
        index,
        repo,
        name,
        session,
        query,
        headers,
        body,
        journal,
    } = request;
    let record = match check_session_chunk(state, index, name, session, headers, body.size_hint().exact())? {
        Ok(record) => record,
        Err(rejection) => return Ok(rejection.into_response()),
    };
    // A `PUT` without a digest cannot commit, but the staged bytes are still good: keep the session so
    // the client can retry with the digest rather than re-upload everything.
    let Some(digest) = query_params(query).remove("digest") else {
        return Ok(error_response(
            ErrorCode::DigestInvalid,
            "finishing an upload requires a digest",
        ));
    };
    let Some(storage) = store::blob_digest(&digest) else {
        return Ok(error_response(
            ErrorCode::DigestInvalid,
            "only sha256 blob digests are supported",
        ));
    };
    let offset = match append_checked_chunk(state, session, record.offset, body, index, repo).await? {
        Ok(offset) => offset,
        Err(rejection) => return Ok(rejection.into_response()),
    };
    commit_staged_upload(
        BlobCommitContext {
            state,
            index,
            repo,
            name,
            digest: &digest,
            bytes: offset,
            journal,
        },
        session,
        storage,
    )
    .await
}

/// The locked read-modify-write shared by `PATCH` and the final `PUT`: re-read the session under the lock
/// so the offset is authoritative, reject an unknown session or an out-of-order chunk, then stream `body`
/// into the durable stage. Returns the new offset, or a response to send unchanged.
async fn append_session_chunk(
    state: &ServingState,
    index: &Index,
    repo: &str,
    name: &str,
    session: &str,
    headers: &HeaderMap,
    body: Body,
) -> Result<Result<u64, RequestRejection>, ServeError> {
    let record = match check_session_chunk(state, index, name, session, headers, body.size_hint().exact())? {
        Ok(record) => record,
        Err(response) => return Ok(Err(response)),
    };
    append_checked_chunk(state, session, record.offset, body, index, repo).await
}

/// Re-read the session under the lock and validate the incoming chunk against it, without touching the
/// stage. Answers 404 for a session this `index`/`name` never opened and 416 for a chunk whose
/// `Content-Range` does not begin where the last one ended or spans a byte count the body cannot honour;
/// a rejected chunk keeps the session's bytes and still counts as activity. Returns the session record
/// positioned at the authoritative offset, or a response to send unchanged.
fn check_session_chunk(
    state: &ServingState,
    index: &Index,
    name: &str,
    session: &str,
    headers: &HeaderMap,
    body_size: Option<u64>,
) -> Result<Result<UploadRecord, RequestRejection>, ServeError> {
    let Some(record) = session_record(state, &index.name, name, session)? else {
        return Ok(Err(
            error_response(ErrorCode::BlobUploadUnknown, "upload unknown").into()
        ));
    };
    if !chunk_range(headers).admits(record.offset, body_size) {
        state.meta.advance_upload(session, record.offset, (state.clock)())?;
        return Ok(Err(range_not_satisfiable(name, session, record.offset).into()));
    }
    Ok(Ok(record))
}

/// Stream a chunk validated by [`check_session_chunk`] into the durable stage, returning the new offset.
async fn append_checked_chunk(
    state: &ServingState,
    session: &str,
    mut offset: u64,
    body: Body,
    index: &Index,
    repo: &str,
) -> Result<Result<u64, RequestRejection>, ServeError> {
    if let Err(err) = append_to_stage(state, session, &mut offset, body, index, repo).await {
        return Ok(Err(append_error_response(state, session, err).await?.into()));
    }
    Ok(Ok(offset))
}

/// The session's durable record, but only when it belongs to this `index` and `name`, so a session id
/// opened by one repository cannot be driven by another.
fn session_record(
    state: &ServingState,
    index: &str,
    name: &str,
    session: &str,
) -> Result<Option<UploadRecord>, ServeError> {
    Ok(state
        .meta
        .upload_record(session)?
        .filter(|record| record.index == index && record.name == name))
}

/// Stream `body` into the session's durable stage, advancing the recorded offset after each chunk lands.
/// A mid-body read error leaves the session recorded at the bytes that reached disk, so a transient
/// hiccup leaves the client a resumable session at that offset instead of forcing a full re-upload.
async fn append_to_stage(
    state: &ServingState,
    session: &str,
    offset: &mut u64,
    body: Body,
    index: &Index,
    repo: &str,
) -> Result<(), UploadBodyError> {
    let mut stream = body.into_data_stream();
    let limit = index.policy.max_artifact_size();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|err| UploadBodyError::Fault(ServeError::Transport(err.to_string())))?;
        let size = *offset + chunk.len() as u64;
        if limit.is_some_and(|limit| size > limit) {
            return Err(UploadBodyError::Denied(
                policy_size_denial(index, repo, size).expect("size above the policy limit is denied"),
            ));
        }
        let staged = state
            .blobs
            .stage_upload_chunk(session, *offset, &chunk)
            .await
            .map_err(ServeError::from)
            .map_err(UploadBodyError::Fault)?;
        *offset = staged;
        state
            .meta
            .advance_upload(session, staged, (state.clock)())
            .map_err(ServeError::from)
            .map_err(UploadBodyError::Fault)?;
    }
    Ok(())
}

enum UploadBodyError {
    Fault(ServeError),
    Denied(Response),
}

impl UploadBodyError {
    fn into_response(self) -> Result<Response, ServeError> {
        match self {
            Self::Fault(err) => Err(err),
            Self::Denied(response) => Ok(response),
        }
    }
}

/// Turn an append failure into its response. A policy rejection ends the upload: its stage and record are
/// dropped. A transient transport fault keeps them, leaving the client a resumable session at the bytes
/// that reached disk instead of forcing a full re-upload.
async fn append_error_response(
    state: &ServingState,
    session: &str,
    err: UploadBodyError,
) -> Result<Response, ServeError> {
    if matches!(err, UploadBodyError::Denied(_)) {
        state.blobs.discard_upload(session).await.map_err(ServeError::from)?;
        state.meta.remove_upload(session)?;
    }
    err.into_response()
}

async fn append_body(
    pending: &mut BlobWrite,
    offset: &mut u64,
    body: Body,
    index: &Index,
    repo: &str,
) -> Result<(), UploadBodyError> {
    let mut stream = body.into_data_stream();
    let limit = index.policy.max_artifact_size();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|err| UploadBodyError::Fault(ServeError::Transport(err.to_string())))?;
        let size = *offset + chunk.len() as u64;
        if limit.is_some_and(|limit| size > limit) {
            return Err(UploadBodyError::Denied(
                policy_size_denial(index, repo, size).expect("size above the policy limit is denied"),
            ));
        }
        pending
            .write_chunk(chunk)
            .await
            .map_err(ServeError::from)
            .map_err(UploadBodyError::Fault)?;
        *offset = size;
    }
    Ok(())
}

/// Attach the `Range` of bytes received so far, reported inclusively as `0-<offset-1>`. A fresh
/// session that has received no bytes reports `0-0`, the value the OCI distribution spec and its
/// conformance suite require on an upload response; omitting the header fails conformance because a
/// client reads the absent header as an empty range with no `0-` prefix.
fn received_range(builder: Builder, offset: u64) -> Builder {
    builder.header(header::RANGE, format!("0-{}", offset.saturating_sub(1)))
}

pub(super) fn created(location: &str, digest: &str) -> Response {
    Response::builder()
        .status(StatusCode::CREATED)
        .header(header::LOCATION, location)
        .header(DOCKER_CONTENT_DIGEST, digest)
        .body(Body::empty())
        .expect("created response builds from validated parts")
}

fn upload_status_response(name: &str, session: &str, offset: u64) -> Response {
    received_range(
        Response::builder()
            .status(StatusCode::NO_CONTENT)
            .header(header::LOCATION, format!("/v2/{name}/blobs/uploads/{session}"))
            .header(DOCKER_UPLOAD_UUID, session),
        offset,
    )
    .body(Body::empty())
    .expect("upload status response builds from validated parts")
}

/// `202 Accepted` for an open upload session, reporting the bytes received so far.
fn upload_accepted(name: &str, session: &str, offset: u64) -> Response {
    received_range(
        Response::builder()
            .status(StatusCode::ACCEPTED)
            .header(header::LOCATION, format!("/v2/{name}/blobs/uploads/{session}"))
            .header(DOCKER_UPLOAD_UUID, session),
        offset,
    )
    .body(Body::empty())
    .expect("upload response builds from validated parts")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkRange {
    /// No `Content-Range`, so the client makes no claim and the chunk appends where the last ended.
    Absent,
    /// A `Content-Range` that is not a well-formed inclusive range: unreadable text, a missing or
    /// non-numeric bound, or an end that precedes its start. The client believes it is resuming
    /// somewhere; it cannot be told it succeeded, because nothing checked where its bytes actually
    /// landed.
    Malformed,
    /// A well-formed inclusive `<start>-<end>`: the chunk continues from `start` and carries `len`
    /// bytes, where `len == end - start + 1`.
    Bytes { start: u64, len: u64 },
}

impl ChunkRange {
    /// Whether a chunk may be appended at `offset`: it claimed nothing, or it claimed an inclusive
    /// range that begins at `offset` and whose `len` matches the body's `size`. A range spanning bytes
    /// the body does not carry (`size` absent or unequal) is refused, so a one-byte `PATCH` cannot
    /// advance the session past its bytes by declaring a wide range.
    fn admits(self, offset: u64, size: Option<u64>) -> bool {
        match self {
            Self::Absent => true,
            Self::Malformed => false,
            Self::Bytes { start, len } => start == offset && size == Some(len),
        }
    }
}

/// Read a chunk's `Content-Range: <start>-<end>` header, tolerating the `bytes ` prefix some clients
/// send. Both bounds are parsed and the span is computed with checked arithmetic, so a reversed range
/// or one whose width overflows `u64` reads as `Malformed` rather than a bogus length.
///
/// Parsing failures used to be indistinguishable from an absent header, which skipped the contiguity
/// check entirely: a chunk claiming to resume at 500 was appended wherever the session happened to be,
/// and the end bound went unread so a wide range advanced the session past the bytes it carried. The
/// final digest check caught the result, but only after the whole upload.
fn chunk_range(headers: &HeaderMap) -> ChunkRange {
    let Some(value) = headers.get(header::CONTENT_RANGE) else {
        return ChunkRange::Absent;
    };
    let Ok(text) = value.to_str() else {
        return ChunkRange::Malformed;
    };
    let trimmed = text.trim();
    let spec = trimmed.strip_prefix("bytes ").unwrap_or(trimmed);
    let Some((start, end)) = spec.split_once('-') else {
        return ChunkRange::Malformed;
    };
    let (Ok(start), Ok(end)) = (start.trim().parse::<u64>(), end.trim().parse::<u64>()) else {
        return ChunkRange::Malformed;
    };
    end.checked_sub(start)
        .and_then(|span| span.checked_add(1))
        .map_or(ChunkRange::Malformed, |len| ChunkRange::Bytes { start, len })
}

/// `416 Range Not Satisfiable` for an out-of-order chunk, reporting the bytes already received. It
/// carries the session's `Location` and `Docker-Upload-UUID` alongside `Range` so a client that sent
/// the chunk out of order has the URL and id to resume against instead of restarting the upload.
fn range_not_satisfiable(name: &str, session: &str, offset: u64) -> Response {
    received_range(
        Response::builder()
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header(header::LOCATION, format!("/v2/{name}/blobs/uploads/{session}"))
            .header(DOCKER_UPLOAD_UUID, session),
        offset,
    )
    .body(Body::empty())
    .expect("range response builds from validated parts")
}

#[cfg(test)]
#[path = "../../tests/unit/registry/uploads/tests.rs"]
mod tests;
