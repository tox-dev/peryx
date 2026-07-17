//! The `POST`/`PATCH`/`PUT` blob-upload session lifecycle.

use super::blobs::{blob_created, blob_fault, commit_blob};
use super::*;
use crate::error::{ErrorCode, error_response};
use crate::store::{self};
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use peryx_driver::ServingState;

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
        let params = query_params(query);
        if let (Some(mount), Some(source)) = (params.get("mount"), params.get("from"))
            && let Some(storage) = store::blob_digest(mount)
        {
            if let Err(response) = auth::authorize_read(state, headers, source) {
                return Ok(response);
            }
            if let Some((source_index, source_repo)) = resolve(&state.indexes, source)
                && !policy_blocks(source_index, PolicyAction::Serve, source_repo)
                && state.blobs.head(&storage).await.map_err(blob_fault)?.is_some()
                && self.blob_authorized(state, source_index, source_repo, mount)?
            {
                if policy_blocks(index, PolicyAction::Upload, &repo) {
                    return Ok(error_response(ErrorCode::Denied, "image name is blocked by policy"));
                }
                store::record_blob_membership(&state.meta, &index.name, &repo, mount)?;
                return Ok(blob_created(name, mount));
            }
        }
        if let Some(digest) = params.get("digest") {
            let mut pending = state.blobs.begin().await.map_err(blob_fault)?;
            let mut size = 0;
            if let Err(err) = append_body(&mut pending, &mut size, body, index, &repo).await {
                return err.into_response();
            }
            return commit_blob(state, pending, &index.name, &repo, name, digest).await;
        }
        let now = (state.clock)();
        let session = Self::random_session()?;
        let pending = state.blobs.begin().await.map_err(blob_fault)?;
        let mut uploads = self.uploads.lock().await;
        let expired = reclaim_expired(&mut uploads, now);
        let session = std::iter::once(Ok(session))
            .chain(std::iter::repeat_with(Self::random_session))
            .find(|candidate| {
                let candidate = candidate.as_ref();
                candidate.is_err() || candidate.is_ok_and(|candidate| !uploads.contains_key(candidate))
            })
            .expect("session candidate iterator cannot end")?;
        uploads.insert(
            session.clone(),
            UploadSession {
                pending,
                offset: 0,
                index: index.name.clone(),
                name: name.to_owned(),
                last_active_at: now,
            },
        );
        drop(uploads);
        abort_uploads(expired).await;
        Ok(upload_accepted(name, &session, 0))
    }

    async fn take_session(&self, index: &str, name: &str, session: &str) -> Option<UploadSession> {
        let mut uploads = self.uploads.lock().await;
        if uploads.get(session).is_none_or(|entry| !entry.belongs_to(index, name)) {
            return None;
        }
        uploads.remove(session)
    }

    /// Stream `body` into a session's staged blob. On a mid-body read error the session is put back
    /// with the bytes that landed, so a transient hiccup leaves the client a resumable session at the
    /// recorded offset instead of forcing a full re-upload.
    async fn append_or_restore(
        &self,
        session: &str,
        mut entry: UploadSession,
        body: Body,
        index: &Index,
        repo: &str,
    ) -> Result<UploadSession, UploadBodyError> {
        match append_body(&mut entry.pending, &mut entry.offset, body, index, repo).await {
            Ok(()) => Ok(entry),
            Err(UploadBodyError::Fault(err)) => {
                self.uploads.lock().await.insert(session.to_owned(), entry);
                Err(UploadBodyError::Fault(err))
            }
            Err(err @ UploadBodyError::Denied(_)) => Err(err),
        }
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
        Ok(match self.take_session(&index.name, name, session).await {
            Some(entry) => {
                entry.pending.abort().await.map_err(blob_fault)?;
                StatusCode::NO_CONTENT.into_response()
            }
            None => error_response(ErrorCode::BlobUploadUnknown, "upload unknown"),
        })
    }

    /// Report an open upload session's progress: `204` with the bytes received so far.
    pub(super) async fn upload_status(
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
        let offset = self
            .uploads
            .lock()
            .await
            .get_mut(session)
            .filter(|entry| entry.belongs_to(&index.name, name))
            .map(|entry| {
                entry.last_active_at = (state.clock)();
                entry.offset
            });
        Ok(offset.map_or_else(
            || error_response(ErrorCode::BlobUploadUnknown, "upload unknown"),
            |offset| upload_status_response(name, session, offset),
        ))
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
        let Some(mut entry) = self.take_session(&index.name, name, session).await else {
            return Ok(error_response(ErrorCode::BlobUploadUnknown, "upload unknown"));
        };
        // The TTL runs from last activity, so this chunk keeps the session alive whether or not it lands.
        entry.last_active_at = (state.clock)();
        // A chunk whose `Content-Range` does not start where the last one ended is out of order, and
        // one whose `Content-Range` cannot be read makes a claim that cannot be honoured. Both answer
        // 416, and the session keeps its bytes so the client can resend.
        if !chunk_start(headers).continues_at(entry.offset) {
            let offset = entry.offset;
            self.uploads.lock().await.insert(session.to_owned(), entry);
            return Ok(range_not_satisfiable(name, session, offset));
        }
        let entry = match self.append_or_restore(session, entry, body, index, &repo).await {
            Ok(entry) => entry,
            Err(err) => return err.into_response(),
        };
        let offset = entry.offset;
        self.uploads.lock().await.insert(session.to_owned(), entry);
        Ok(upload_accepted(name, session, offset))
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
        let Some(entry) = self.take_session(&index.name, name, session).await else {
            return Ok(error_response(ErrorCode::BlobUploadUnknown, "upload unknown"));
        };
        // A final chunk carrying a `Content-Range` must also be contiguous, exactly like a `PATCH`.
        if !chunk_start(headers).continues_at(entry.offset) {
            let offset = entry.offset;
            self.uploads.lock().await.insert(session.to_owned(), entry);
            return Ok(range_not_satisfiable(name, session, offset));
        }
        let entry = match self.append_or_restore(session, entry, body, index, &repo).await {
            Ok(entry) => entry,
            Err(err) => return err.into_response(),
        };
        // A `PUT` without a digest cannot commit, but the staged bytes are still good: keep the
        // session so the client can retry with the digest rather than re-upload everything.
        let Some(digest) = query_params(query).remove("digest") else {
            self.uploads.lock().await.insert(session.to_owned(), entry);
            return Ok(error_response(
                ErrorCode::DigestInvalid,
                "finishing an upload requires a digest",
            ));
        };
        commit_blob(state, entry.pending, &index.name, &repo, name, &digest).await
    }
}

pub(super) fn reclaim_expired(
    uploads: &mut std::collections::HashMap<String, UploadSession>,
    now: i64,
) -> Vec<UploadSession> {
    uploads
        .extract_if(|_, session| now.saturating_sub(session.last_active_at) >= UPLOAD_SESSION_TTL_SECS)
        .map(|(_, session)| session)
        .collect()
}

pub(super) async fn abort_uploads(uploads: Vec<UploadSession>) {
    for upload in uploads {
        let _ = upload.pending.abort().await;
    }
}

impl UploadSession {
    fn belongs_to(&self, index: &str, name: &str) -> bool {
        self.index == index && self.name == name
    }
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
