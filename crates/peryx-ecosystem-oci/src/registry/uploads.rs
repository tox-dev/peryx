//! The `POST`/`PATCH`/`PUT` blob-upload session lifecycle.

use super::blobs::{blob_created, blob_fault, commit_blob, stream_into};
use super::*;
use crate::error::{ErrorCode, error_response};
use crate::store::{self};
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use peryx_driver::ServingState;

impl OciRegistry {
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
        let (index, repo) = match resolve_writable(state, name, headers) {
            Ok(pair) => pair,
            Err(response) => return Ok(response),
        };
        let params = query_params(query);
        if let Some(mount) = params.get("mount")
            && store::blob_digest(mount).is_some_and(|storage| state.blobs.exists(&storage))
        {
            if policy_blocks(index, PolicyAction::Upload, &repo) {
                return Ok(error_response(ErrorCode::Denied, "image name is blocked by policy"));
            }
            return Ok(blob_created(name, mount));
        }
        if let Some(digest) = params.get("digest") {
            let mut pending = state.blobs.begin().map_err(blob_fault)?;
            let size = stream_into(&mut pending, body).await?;
            if let Some(response) = policy_size_denial(index, &repo, size) {
                return Ok(response);
            }
            return Ok(commit_blob(&state.blobs, pending, name, digest));
        }
        let pending = state.blobs.begin().map_err(blob_fault)?;
        let session = self.new_session();
        let now = (state.clock)();
        let entry = UploadSession {
            pending,
            offset: 0,
            index: index.name.clone(),
            last_active_at: now,
        };
        let mut uploads = self.uploads.lock().await;
        uploads.retain(|_, session| now - session.last_active_at < UPLOAD_SESSION_TTL_SECS);
        uploads.insert(session.clone(), entry);
        drop(uploads);
        Ok(upload_accepted(name, &session, 0))
    }

    /// Remove an open session by id, but only when `index` is the one that opened it, so a client
    /// authorized for its own index cannot take or disrupt another index's upload by guessing the id.
    async fn take_session(&self, index: &str, session: &str) -> Option<UploadSession> {
        let mut uploads = self.uploads.lock().await;
        if uploads.get(session).is_none_or(|entry| entry.index != index) {
            return None;
        }
        uploads.remove(session)
    }

    /// Report an open upload session's progress: `204` with the bytes received so far.
    pub(super) async fn upload_status(
        &self,
        state: &ServingState,
        headers: &HeaderMap,
        name: &str,
        session: &str,
    ) -> Result<Response, ServeError> {
        let (index, _) = match resolve_writable(state, name, headers) {
            Ok(pair) => pair,
            Err(response) => return Ok(response),
        };
        let offset = self
            .uploads
            .lock()
            .await
            .get_mut(session)
            .filter(|entry| entry.index == index.name)
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
        let (index, _) = match resolve_writable(state, name, headers) {
            Ok(pair) => pair,
            Err(response) => return Ok(response),
        };
        let Some(mut entry) = self.take_session(&index.name, session).await else {
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
            return Ok(range_not_satisfiable(offset));
        }
        entry.offset += stream_into(&mut entry.pending, body).await?;
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
        let (index, repo) = match resolve_writable(state, name, headers) {
            Ok(pair) => pair,
            Err(response) => return Ok(response),
        };
        let Some(mut entry) = self.take_session(&index.name, session).await else {
            return Ok(error_response(ErrorCode::BlobUploadUnknown, "upload unknown"));
        };
        // A final chunk carrying a `Content-Range` must also be contiguous, exactly like a `PATCH`.
        if !chunk_start(headers).continues_at(entry.offset) {
            let offset = entry.offset;
            self.uploads.lock().await.insert(session.to_owned(), entry);
            return Ok(range_not_satisfiable(offset));
        }
        entry.offset += stream_into(&mut entry.pending, body).await?;
        let Some(digest) = query_params(query).remove("digest") else {
            return Ok(error_response(
                ErrorCode::DigestInvalid,
                "finishing an upload requires a digest",
            ));
        };
        if let Some(response) = policy_size_denial(index, &repo, entry.offset) {
            return Ok(response);
        }
        Ok(commit_blob(&state.blobs, entry.pending, name, &digest))
    }
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

/// `416 Range Not Satisfiable` for an out-of-order chunk, reporting the bytes already received.
fn range_not_satisfiable(offset: u64) -> Response {
    Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
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
