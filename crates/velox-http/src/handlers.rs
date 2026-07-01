//! axum request handlers.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use velox_core::pypi::{ProjectDetail, ProjectList, normalize_name, render_detail_html, render_index_html, to_json};
use velox_storage::blob::Digest;

use crate::cache::{self, CacheError};
use crate::state::AppState;

const MIME_JSON: &str = "application/vnd.pypi.simple.v1+json";
const MIME_HTML: &str = "text/html; charset=utf-8";

#[derive(Clone, Copy)]
pub(crate) enum Format {
    Json,
    Html,
}

fn negotiate(headers: &HeaderMap) -> Format {
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if accept.contains("json") {
        Format::Json
    } else {
        Format::Html
    }
}

/// `GET /{user}/{index}/simple/` — the observed project list.
pub async fn simple_index(
    State(state): State<Arc<AppState>>,
    Path((user, index)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    state.requests.fetch_add(1, Ordering::Relaxed);
    if !state.matches_index(&user, &index) {
        return (StatusCode::NOT_FOUND, "unknown index").into_response();
    }
    index_response(cache::project_list(&state), negotiate(&headers))
}

/// Map a project-list result to a negotiated response. Sync so every arm is directly testable.
pub(crate) fn index_response(result: Result<ProjectList, CacheError>, format: Format) -> Response {
    let Ok(list) = result else {
        return (StatusCode::BAD_GATEWAY, "index error").into_response();
    };
    let vary = (header::VARY, "Accept");
    match format {
        Format::Json => ([(header::CONTENT_TYPE, MIME_JSON), vary], to_json(&list)).into_response(),
        Format::Html => ([(header::CONTENT_TYPE, MIME_HTML), vary], render_index_html(&list)).into_response(),
    }
}

/// `GET /{user}/{index}/simple/{project}/` — the project detail page.
pub async fn simple_detail(
    State(state): State<Arc<AppState>>,
    Path((user, index, project)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    state.requests.fetch_add(1, Ordering::Relaxed);
    if !state.matches_index(&user, &index) {
        return (StatusCode::NOT_FOUND, "unknown index").into_response();
    }
    detail_response(
        cache::project_detail(&state, &normalize_name(&project)).await,
        negotiate(&headers),
    )
}

/// Map a resolved project detail to a negotiated response. Kept sync so every arm is directly
/// unit-testable.
pub(crate) fn detail_response(result: Result<Option<ProjectDetail>, CacheError>, format: Format) -> Response {
    let detail = match result {
        Ok(Some(detail)) => detail,
        Ok(None) => return (StatusCode::NOT_FOUND, "project not found").into_response(),
        Err(err) => {
            tracing::error!(error = ?err, "upstream error");
            return (StatusCode::BAD_GATEWAY, "upstream error").into_response();
        }
    };
    let vary = (header::VARY, "Accept");
    match format {
        Format::Json => ([(header::CONTENT_TYPE, MIME_JSON), vary], to_json(&detail)).into_response(),
        Format::Html => ([(header::CONTENT_TYPE, MIME_HTML), vary], render_detail_html(&detail)).into_response(),
    }
}

/// `GET /{user}/{index}/files/{sha256}/{filename}` — a cached (or lazily fetched) blob. A
/// `{filename}.metadata` request serves the wheel's PEP 658 metadata sibling instead.
pub async fn file_download(
    State(state): State<Arc<AppState>>,
    Path((user, index, sha256, filename)): Path<(String, String, String, String)>,
) -> Response {
    state.requests.fetch_add(1, Ordering::Relaxed);
    if !state.matches_index(&user, &index) {
        return (StatusCode::NOT_FOUND, "unknown index").into_response();
    }
    let Some(digest) = Digest::from_hex(&sha256) else {
        return (StatusCode::BAD_REQUEST, "invalid digest").into_response();
    };
    if filename.ends_with(".metadata") {
        state.metadata_requests.fetch_add(1, Ordering::Relaxed);
        file_response(cache::metadata_bytes(&state, &digest).await)
    } else {
        file_response(cache::file_bytes(&state, &digest).await)
    }
}

/// Map a file-bytes result to a response. Sync so every arm is directly unit-testable.
pub(crate) fn file_response(result: Result<bytes::Bytes, CacheError>) -> Response {
    match result {
        Ok(body) => (
            [
                (header::CONTENT_TYPE, "application/octet-stream"),
                (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
            ],
            body,
        )
            .into_response(),
        Err(CacheError::FileNotFound) => (StatusCode::NOT_FOUND, "file not found").into_response(),
        Err(_) => (StatusCode::BAD_GATEWAY, "upstream error").into_response(),
    }
}

/// `GET /+status` — health and identity.
pub async fn status(State(state): State<Arc<AppState>>) -> Response {
    let serial = state.meta.current_serial().unwrap_or(0);
    axum::Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "index": state.index,
        "serial": serial,
    }))
    .into_response()
}

/// `GET /metrics` — Prometheus text exposition.
pub async fn metrics(State(state): State<Arc<AppState>>) -> Response {
    let requests = state.requests.load(Ordering::Relaxed);
    let metadata = state.metadata_requests.load(Ordering::Relaxed);
    let body = format!(
        "# HELP velox_requests_total Total HTTP requests served.\n\
         # TYPE velox_requests_total counter\n\
         velox_requests_total {requests}\n\
         # HELP velox_metadata_requests_total PEP 658 .metadata siblings served.\n\
         # TYPE velox_metadata_requests_total counter\n\
         velox_metadata_requests_total {metadata}\n"
    );
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], body).into_response()
}
