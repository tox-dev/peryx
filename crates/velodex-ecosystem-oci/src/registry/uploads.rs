//! The `POST`/`PATCH`/`PUT` blob-upload session lifecycle.

use super::blobs::{blob_created, blob_fault, commit_blob, stream_into};
use super::*;
use crate::error::{ErrorCode, error_response};
use crate::store::{self};
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use velodex_http::AppState;

impl OciRegistry {
    /// Begin a blob upload: cross-repo mount when the blob is already stored, a monolithic write when
    /// the `POST` carries a `digest`, otherwise a session the client fills with `PATCH`/`PUT`.
    pub(super) async fn start_upload(
        &self,
        state: &AppState,
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
            created_at: now,
        };
        let mut uploads = self.uploads.lock().await;
        uploads.retain(|_, session| now - session.created_at < UPLOAD_SESSION_TTL_SECS);
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
        state: &AppState,
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
            .get(session)
            .filter(|entry| entry.index == index.name)
            .map(|entry| entry.offset);
        Ok(offset.map_or_else(
            || error_response(ErrorCode::BlobUploadUnknown, "upload unknown"),
            |offset| upload_status_response(name, session, offset),
        ))
    }

    /// Append a chunk to an open upload session.
    pub(super) async fn patch_upload(
        &self,
        state: &AppState,
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
        // A chunk whose `Content-Range` does not start where the last one ended is out of order; the
        // spec answers 416 and the session keeps its bytes so the client can resend.
        if let Some(start) = content_range_start(headers)
            && start != entry.offset
        {
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
        state: &AppState,
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
        if let Some(start) = content_range_start(headers)
            && start != entry.offset
        {
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

/// The start byte of a chunk's `Content-Range: <start>-<end>` header, if present.
fn content_range_start(headers: &HeaderMap) -> Option<u64> {
    headers
        .get(header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())?
        .split('-')
        .next()?
        .trim()
        .parse()
        .ok()
}

/// `416 Range Not Satisfiable` for an out-of-order chunk, reporting the bytes already received.
fn range_not_satisfiable(offset: u64) -> Response {
    Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(header::RANGE, format!("0-{}", offset.saturating_sub(1)))
        .body(Body::empty())
        .expect("range response builds from validated parts")
}
