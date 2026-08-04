//! The `POST`/`PATCH`/`PUT` blob-upload session lifecycle.

use super::blobs::{blob_created, blob_fault, commit_blob, commit_staged_upload};
use super::*;
use crate::error::{ErrorCode, error_response};
use crate::store::{self};
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use peryx_driver::ServingState;
use peryx_storage::meta::UploadRecord;

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
        let (index, repo, _) = match resolve_writable(state, name, headers, Action::Write) {
            Ok(target) => target,
            Err(response) => return Ok(response),
        };
        let journal = self.journal_outbox;
        let params = query_params(query);
        if let (Some(mount), Some(source)) = (params.get("mount"), params.get("from"))
            && let Some(storage) = store::blob_digest(mount)
        {
            if let Err(response) = auth::authorize_read(state, headers, source) {
                return Ok(response);
            }
            if let Some((source_index, source_repo)) = resolve(&state.indexes, source)
                && !policy_blocks(source_index, PolicyAction::Serve, source_repo)
                && let Some(metadata) = state.blobs.head(&storage).await.map_err(blob_fault)?
                && self.blob_authorized(state, source_index, source_repo, mount)?
            {
                if policy_blocks(index, PolicyAction::Upload, &repo) {
                    return Ok(error_response(ErrorCode::Denied, "image name is blocked by policy"));
                }
                if let Some(response) = policy_size_denial(index, &repo, metadata.bytes) {
                    return Ok(response);
                }
                // A mount publishes an existing blob into this repository without a transfer, so it
                // reserves the mounted digest's bytes exactly as an upload of them would; a digest
                // already served here is not reserved again.
                let reservation = if store::blob_is_member(&state.meta, &index.name, &repo, mount)? {
                    None
                } else {
                    match crate::quota::admit_push(state, index, &repo, None, mount, metadata.bytes)? {
                        crate::quota::Admission::Rejected(response) => return Ok(response),
                        crate::quota::Admission::Unmetered => None,
                        crate::quota::Admission::Reserved(record) => Some(record),
                    }
                };
                crate::quota::commit_blob_membership(&state.meta, &index.name, &repo, mount, reservation, journal)?;
                return Ok(blob_created(name, mount));
            }
        }
        if let Some(digest) = params.get("digest") {
            let mut pending = state.blobs.begin().await.map_err(blob_fault)?;
            let mut size = 0;
            if let Err(err) = append_body(&mut pending, &mut size, body, index, &repo).await {
                return err.into_response();
            }
            return commit_blob(state, pending, index, &repo, name, digest, size, journal).await;
        }
        let now = (state.clock)();
        let session = Self::random_session()?;
        // The session is durable from the first byte: a restart between chunks recovers it from the
        // store and resumes at the recorded offset rather than losing the upload. The stage is opened
        // empty now so a zero-byte upload finalized without any chunk still has bytes to verify.
        state.meta.begin_upload(&session, &index.name, name, now)?;
        state
            .blobs
            .stage_upload_chunk(&session, 0, b"")
            .await
            .map_err(blob_fault)?;
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
            Err(response) => return Ok(response),
        };
        let Some(_record) = session_record(state, &index.name, name, session)? else {
            return Ok(error_response(ErrorCode::BlobUploadUnknown, "upload unknown"));
        };
        state.blobs.discard_upload(session).await.map_err(blob_fault)?;
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
            Err(response) => return Ok(response),
        };
        let Some(record) = session_record(state, &index.name, name, session)? else {
            return Ok(error_response(ErrorCode::BlobUploadUnknown, "upload unknown"));
        };
        // A status read is activity, so it keeps the session alive against the idle TTL.
        state.meta.advance_upload(session, record.offset, (state.clock)())?;
        Ok(upload_status_response(name, session, record.offset))
    }

    /// Append a chunk to an open upload session.
    pub(super) async fn patch_upload(
        &self,
        state: &ServingState,
        headers: &HeaderMap,
        name: &str,
        session: &str,
        body: Body,
    ) -> Result<Response, ServeError> {
        let (index, repo, _) = match resolve_writable(state, name, headers, Action::Write) {
            Ok(target) => target,
            Err(response) => return Ok(response),
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

    /// Finish an upload: append any trailing bytes, then verify and commit under the given `digest`.
    pub(super) async fn finish_upload(
        &self,
        state: &ServingState,
        headers: &HeaderMap,
        query: &str,
        name: &str,
        session: &str,
        body: Body,
    ) -> Result<Response, ServeError> {
        let (index, repo, _) = match resolve_writable(state, name, headers, Action::Write) {
            Ok(target) => target,
            Err(response) => return Ok(response),
        };
        let lock = self.session_gate.lock(session);
        let outcome = {
            let _guard = lock.lock_owned().await;
            finish_locked(
                state,
                index,
                &repo,
                name,
                session,
                query,
                headers,
                body,
                self.journal_outbox,
            )
            .await
        };
        self.session_gate.release(session);
        outcome
    }
}

/// Append a chunk under the session lock and answer `202`. Run inside the per-session guard.
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
        Err(response) => Ok(response),
    }
}

/// Append any trailing bytes under the session lock, then verify and commit under the given `digest`.
/// Run inside the per-session guard so the append and the commit see a stage no other writer can touch.
#[allow(
    clippy::too_many_arguments,
    reason = "the final PUT threads the request, session, and commit context"
)]
async fn finish_locked(
    state: &ServingState,
    index: &Index,
    repo: &str,
    name: &str,
    session: &str,
    query: &str,
    headers: &HeaderMap,
    body: Body,
    journal_outbox: bool,
) -> Result<Response, ServeError> {
    let offset = match append_session_chunk(state, index, repo, name, session, headers, body).await? {
        Ok(offset) => offset,
        Err(response) => return Ok(response),
    };
    // A `PUT` without a digest cannot commit, but the staged bytes are still good: keep the session so
    // the client can retry with the digest rather than re-upload everything.
    let Some(digest) = query_params(query).remove("digest") else {
        return Ok(error_response(
            ErrorCode::DigestInvalid,
            "finishing an upload requires a digest",
        ));
    };
    commit_staged_upload(state, session, index, repo, name, &digest, offset, journal_outbox).await
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
) -> Result<Result<u64, Response>, ServeError> {
    let Some(record) = session_record(state, &index.name, name, session)? else {
        return Ok(Err(error_response(ErrorCode::BlobUploadUnknown, "upload unknown")));
    };
    // A chunk whose `Content-Range` does not start where the last one ended is out of order, and one
    // whose `Content-Range` cannot be read makes a claim that cannot be honoured. Both answer 416, and the
    // session keeps its bytes so the client can resend; the read still counts as activity.
    if !chunk_start(headers).continues_at(record.offset) {
        state.meta.advance_upload(session, record.offset, (state.clock)())?;
        return Ok(Err(range_not_satisfiable(name, session, record.offset)));
    }
    let mut offset = record.offset;
    if let Err(err) = append_to_stage(state, session, &mut offset, body, index, repo).await {
        return Ok(Err(append_error_response(state, session, err).await?));
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
    let limit = index.policy.max_file_size();
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
            .map_err(blob_fault)
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
        state.blobs.discard_upload(session).await.map_err(blob_fault)?;
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
    let limit = index.policy.max_file_size();
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
            .map_err(blob_fault)
            .map_err(UploadBodyError::Fault)?;
        *offset = size;
    }
    Ok(())
}

/// A `201 Created` carrying a `Location` and the canonical `Docker-Content-Digest`.
pub(super) fn created(location: &str, digest: &str) -> Response {
    Response::builder()
        .status(StatusCode::CREATED)
        .header(header::LOCATION, location)
        .header(DOCKER_CONTENT_DIGEST, digest)
        .body(Body::empty())
        .expect("created response builds from validated parts")
}

/// `204 No Content` reporting an open upload session's progress.
fn upload_status_response(name: &str, session: &str, offset: u64) -> Response {
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header(header::LOCATION, format!("/v2/{name}/blobs/uploads/{session}"))
        .header(DOCKER_UPLOAD_UUID, session)
        .header(header::RANGE, format!("0-{}", offset.saturating_sub(1)))
        .body(Body::empty())
        .expect("upload status response builds from validated parts")
}

/// `202 Accepted` for an open upload session, reporting the bytes received so far.
fn upload_accepted(name: &str, session: &str, offset: u64) -> Response {
    Response::builder()
        .status(StatusCode::ACCEPTED)
        .header(header::LOCATION, format!("/v2/{name}/blobs/uploads/{session}"))
        .header(DOCKER_UPLOAD_UUID, session)
        .header(header::RANGE, format!("0-{}", offset.saturating_sub(1)))
        .body(Body::empty())
        .expect("upload response builds from validated parts")
}

/// Where a chunk says it begins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChunkStart {
    /// No `Content-Range`, so the client makes no claim and the chunk appends where the last ended.
    Absent,
    /// A `Content-Range` that is not a range. The client believes it is resuming somewhere; it cannot
    /// be told it succeeded, because nothing checked where its bytes actually landed.
    Malformed,
    /// The offset the client says this chunk continues from.
    At(u64),
}

impl ChunkStart {
    /// Whether a chunk may be appended at `offset`: it claimed that offset, or claimed nothing.
    const fn continues_at(self, offset: u64) -> bool {
        match self {
            Self::Absent => true,
            Self::At(start) => start == offset,
            Self::Malformed => false,
        }
    }
}

/// Read a chunk's `Content-Range: <start>-<end>` header, tolerating the `bytes ` prefix some clients
/// send.
///
/// Parsing failures used to be indistinguishable from an absent header, which skipped the contiguity
/// check entirely: a chunk claiming to resume at 500 was appended wherever the session happened to be.
/// The final digest check caught the result, but only after the whole upload.
fn chunk_start(headers: &HeaderMap) -> ChunkStart {
    let Some(value) = headers.get(header::CONTENT_RANGE) else {
        return ChunkStart::Absent;
    };
    let Ok(text) = value.to_str() else {
        return ChunkStart::Malformed;
    };
    let trimmed = text.trim();
    let spec = trimmed.strip_prefix("bytes ").unwrap_or(trimmed);
    let Some((start, _)) = spec.split_once('-') else {
        return ChunkStart::Malformed;
    };
    start.trim().parse().map_or(ChunkStart::Malformed, ChunkStart::At)
}

/// `416 Range Not Satisfiable` for an out-of-order chunk, reporting the bytes already received. It
/// carries the session's `Location` and `Docker-Upload-UUID` alongside `Range` so a client that sent
/// the chunk out of order has the URL and id to resume against instead of restarting the upload.
fn range_not_satisfiable(name: &str, session: &str, offset: u64) -> Response {
    Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(header::LOCATION, format!("/v2/{name}/blobs/uploads/{session}"))
        .header(DOCKER_UPLOAD_UUID, session)
        .header(header::RANGE, format!("0-{}", offset.saturating_sub(1)))
        .body(Body::empty())
        .expect("range response builds from validated parts")
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::{ChunkStart, chunk_start};

    fn headers(value: HeaderValue) -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(axum::http::header::CONTENT_RANGE, value);
        headers
    }

    #[test]
    fn test_chunk_start_reads_an_offset_with_or_without_the_bytes_prefix() {
        assert_eq!(
            chunk_start(&headers(HeaderValue::from_static("5-9"))),
            ChunkStart::At(5)
        );
        assert_eq!(
            chunk_start(&headers(HeaderValue::from_static("bytes 5-9"))),
            ChunkStart::At(5)
        );
    }

    #[test]
    fn test_chunk_start_rejects_a_header_that_is_not_a_range() {
        // A `Content-Range` whose bytes are not text at all: the client made a claim nothing can read.
        let opaque = HeaderValue::from_bytes(&[0xff, 0xfe]).expect("bytes are a valid header value");
        assert_eq!(chunk_start(&headers(opaque)), ChunkStart::Malformed);
        assert_eq!(
            chunk_start(&headers(HeaderValue::from_static("nowhere"))),
            ChunkStart::Malformed
        );
    }

    #[test]
    fn test_chunk_start_is_absent_without_the_header() {
        assert_eq!(chunk_start(&axum::http::HeaderMap::new()), ChunkStart::Absent);
    }
}
