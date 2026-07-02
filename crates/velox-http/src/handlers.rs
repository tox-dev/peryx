//! axum request handlers.
//!
//! All index traffic arrives on a catch-all path that is resolved to a configured index by longest
//! route prefix, then dispatched by method and remainder.
#![allow(
    clippy::result_large_err,
    reason = "handler helpers carry an axum Response as their error; boxing it everywhere adds noise"
)]

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::{Multipart, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use velox_core::pypi::{ProjectDetail, ProjectList, normalize_name, render_detail_html, render_index_html, to_json};
use velox_storage::blob::Digest;

use crate::cache::{self, CacheError};
use crate::state::{AppState, Index, IndexKind};
use crate::upload::{self, UploadError, UploadForm};

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

/// `GET /{route}/...` — project list, project detail, or a file/metadata download.
pub async fn dispatch_get(
    State(state): State<Arc<AppState>>,
    Path(path): Path<String>,
    headers: HeaderMap,
) -> Response {
    state.requests.fetch_add(1, Ordering::Relaxed);
    let Some((index, rest)) = state.resolve(&path) else {
        return not_found();
    };
    if rest == "simple/" {
        return index_response(cache::resolve_list(&state, index), negotiate(&headers));
    }
    if let Some(project) = rest.strip_prefix("simple/").and_then(|rest| rest.strip_suffix('/')) {
        let normalized = normalize_name(project);
        let detail = cache::resolve_detail(&state, index, &normalized, &index.route).await;
        return detail_response(detail, negotiate(&headers));
    }
    if let Some(file) = rest.strip_prefix("files/") {
        return file_route(&state, file).await;
    }
    if let Some(target) = rest.strip_prefix("inspect/") {
        return inspect_route(&state, target).await;
    }
    not_found()
}

/// `GET /{route}/inspect/{sha256}/{filename}[/{member}]` — list a cached archive's members, or read
/// one member inline (pypi-browser-style introspection over the blob store).
async fn inspect_route(state: &AppState, target: &str) -> Response {
    let Some((sha256, rest)) = target.split_once('/') else {
        return not_found();
    };
    let Some(digest) = Digest::from_hex(sha256) else {
        return (StatusCode::BAD_REQUEST, "invalid digest").into_response();
    };
    let (filename, member) = match rest.split_once('/') {
        Some((filename, member)) => (filename, Some(member)),
        None => (rest, None),
    };
    let bytes = match cache::file_bytes(state, &digest).await {
        Ok(bytes) => bytes,
        Err(err) => return file_response(Err(err)),
    };
    member.map_or_else(
        || archive_listing(filename, &bytes),
        |member| archive_member(filename, &bytes, member),
    )
}

/// Render an archive's member list as JSON.
fn archive_listing(filename: &str, bytes: &[u8]) -> Response {
    match crate::archive::list_members(filename, bytes) {
        Ok(members) => axum::Json(serde_json::json!({ "filename": filename, "members": members })).into_response(),
        Err(err) => archive_error(&err),
    }
}

/// Serve one archive member: UTF-8 content as text, anything else as bytes.
fn archive_member(filename: &str, bytes: &[u8], member: &str) -> Response {
    match crate::archive::read_member(filename, bytes, member) {
        Ok(content) => match String::from_utf8(content) {
            Ok(text) => ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], text).into_response(),
            Err(raw) => ([(header::CONTENT_TYPE, "application/octet-stream")], raw.into_bytes()).into_response(),
        },
        Err(err) => archive_error(&err),
    }
}

fn archive_error(err: &crate::archive::ArchiveError) -> Response {
    use crate::archive::ArchiveError;
    let status = match err {
        ArchiveError::Unsupported => StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ArchiveError::MemberNotFound => StatusCode::NOT_FOUND,
        ArchiveError::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        ArchiveError::Read(_) => StatusCode::UNPROCESSABLE_ENTITY,
    };
    (status, err.to_string()).into_response()
}

async fn file_route(state: &AppState, file: &str) -> Response {
    let Some((sha256, filename)) = file.split_once('/') else {
        return not_found();
    };
    let Some(digest) = Digest::from_hex(sha256) else {
        return (StatusCode::BAD_REQUEST, "invalid digest").into_response();
    };
    if filename.ends_with(".metadata") {
        state.metadata_requests.fetch_add(1, Ordering::Relaxed);
        file_response(cache::metadata_bytes(state, &digest).await)
    } else {
        file_response(cache::file_bytes(state, &digest).await)
    }
}

/// `POST /{route}/` — the legacy multipart upload API, used unchanged by twine and `uv publish`.
pub async fn dispatch_post(
    State(state): State<Arc<AppState>>,
    Path(path): Path<String>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Response {
    state.requests.fetch_add(1, Ordering::Relaxed);
    let Some((index, rest)) = state.resolve(&path) else {
        return not_found();
    };
    if !rest.is_empty() {
        return not_found();
    }
    let Some(local) = upload_target(&state, index) else {
        return (StatusCode::METHOD_NOT_ALLOWED, "index does not accept uploads").into_response();
    };
    if let Err(response) = authorize(local, &headers) {
        return response;
    }
    let form = match collect_form(multipart).await {
        Ok(form) => form,
        Err(response) => return response,
    };
    let prepared = match upload::prepare(form, &index.route) {
        Ok(prepared) => prepared,
        Err(err) => return upload_error_response(&err),
    };
    match cache::store_upload(&state, &local.name, &prepared) {
        Ok(()) => (StatusCode::OK, "upload accepted").into_response(),
        Err(err) => {
            tracing::error!(error = ?err, "upload store failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "storage error").into_response()
        }
    }
}

/// `PUT /{route}/{project}/[{version}/]yank` marks files yanked (PEP 592, reversible);
/// `PUT .../restore` clears the hidden marker a DELETE left on read-only upstream files.
pub async fn dispatch_put(
    State(state): State<Arc<AppState>>,
    Path(path): Path<String>,
    headers: HeaderMap,
) -> Response {
    state.requests.fetch_add(1, Ordering::Relaxed);
    let (index, local, spec) = match removal_target(&state, &path, &headers) {
        Ok(target) => target,
        Err(response) => return response,
    };
    if let Some(spec) = spec.strip_suffix("yank") {
        let (project, version) = parse_project_version(spec);
        return count_response(cache::set_yanked(&state, index, &local.name, &project, version.as_deref(), true).await);
    }
    if let Some(spec) = spec.strip_suffix("restore") {
        let (project, version) = parse_project_version(spec);
        return count_response(cache::restore_files(&state, &local.name, &project, version.as_deref()));
    }
    not_found()
}

/// `DELETE /{route}/{project}/[{version}/]` removes files: uploads are deleted outright (volatile
/// indexes only), read-only upstream files are hidden reversibly. A `.../yank` suffix un-yanks.
pub async fn dispatch_delete(
    State(state): State<Arc<AppState>>,
    Path(path): Path<String>,
    headers: HeaderMap,
) -> Response {
    state.requests.fetch_add(1, Ordering::Relaxed);
    let (index, local, spec) = match removal_target(&state, &path, &headers) {
        Ok(target) => target,
        Err(response) => return response,
    };
    if let Some(spec) = spec.strip_suffix("yank") {
        let (project, version) = parse_project_version(spec);
        return count_response(
            cache::set_yanked(&state, index, &local.name, &project, version.as_deref(), false).await,
        );
    }
    let (project, version) = parse_project_version(spec);
    let volatile = is_volatile(local);
    count_response(cache::remove_files(&state, index, &local.name, volatile, &project, version.as_deref()).await)
}

/// Resolve the writable local index for a mutation request and authorize it, returning the serving
/// index, its local layer, and the path remainder (the `{project}/...` part).
fn removal_target<'a>(
    state: &'a AppState,
    path: &'a str,
    headers: &HeaderMap,
) -> Result<(&'a Index, &'a Index, &'a str), Response> {
    let (index, rest) = state.resolve(path).ok_or_else(not_found)?;
    let local = upload_target(state, index)
        .ok_or_else(|| (StatusCode::METHOD_NOT_ALLOWED, "index is read-only").into_response())?;
    authorize(local, headers)?;
    Ok((index, local, rest))
}

/// The writable local index behind `index`: itself if local, its upload layer if an overlay.
fn upload_target<'a>(state: &'a AppState, index: &'a Index) -> Option<&'a Index> {
    match &index.kind {
        IndexKind::Local { .. } => Some(index),
        IndexKind::Overlay { upload: Some(pos), .. } => Some(state.index_at(*pos)),
        IndexKind::Mirror(_) | IndexKind::Overlay { upload: None, .. } => None,
    }
}

const fn is_volatile(local: &Index) -> bool {
    matches!(local.kind, IndexKind::Local { volatile: true, .. })
}

/// Check the Basic-auth token of a local index, returning a ready response on any failure.
fn authorize(local: &Index, headers: &HeaderMap) -> Result<(), Response> {
    let IndexKind::Local { upload_token, .. } = &local.kind else {
        return Err(not_found());
    };
    let Some(token) = upload_token.as_deref() else {
        return Err((StatusCode::FORBIDDEN, "uploads are disabled").into_response());
    };
    let auth = headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok());
    if upload::authorized(auth, token) {
        Ok(())
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"velox\"")],
            "unauthorized",
        )
            .into_response())
    }
}

fn parse_project_version(spec: &str) -> (String, Option<String>) {
    let trimmed = spec.trim_matches('/');
    let mut parts = trimmed.splitn(2, '/');
    let project = normalize_name(parts.next().unwrap_or_default());
    let version = parts
        .next()
        .map(|version| version.trim_matches('/').to_owned())
        .filter(|version| !version.is_empty());
    (project, version)
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

fn count_response(result: Result<usize, CacheError>) -> Response {
    match result {
        Ok(0) => (StatusCode::NOT_FOUND, "nothing to remove").into_response(),
        Ok(count) => (StatusCode::OK, format!("affected {count} file(s)")).into_response(),
        Err(CacheError::NotVolatile) => {
            (StatusCode::FORBIDDEN, "index is not volatile; delete is disabled").into_response()
        }
        Err(err) => {
            tracing::error!(error = ?err, "removal failed");
            (StatusCode::INTERNAL_SERVER_ERROR, "storage error").into_response()
        }
    }
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}

/// Drain a multipart body into an [`UploadForm`], reading the `content` part as bytes and the rest
/// as UTF-8 text. Unknown fields are ignored, as the upload API carries many metadata fields velox
/// does not need. Every read or decode error funnels through [`reject`] as a 400.
async fn collect_form(mut multipart: Multipart) -> Result<UploadForm, Response> {
    let mut form = UploadForm::default();
    while let Some(field) = multipart.next_field().await.map_err(reject)? {
        let name = field.name().unwrap_or_default().to_owned();
        if name == "content" {
            form.filename = field.file_name().map(str::to_owned);
            form.content = Some(field.bytes().await.map_err(reject)?.to_vec());
        } else {
            let value = String::from_utf8(field.bytes().await.map_err(reject)?.to_vec()).map_err(reject)?;
            match name.as_str() {
                ":action" => form.action = Some(value),
                "name" => form.name = Some(value),
                "version" => form.version = Some(value),
                "requires_python" => form.requires_python = Some(value),
                "sha256_digest" => form.sha256_digest = Some(value),
                _ => {}
            }
        }
    }
    Ok(form)
}

/// Map any multipart read or decode failure to a 400 response.
fn reject(err: impl std::fmt::Display) -> Response {
    (StatusCode::BAD_REQUEST, format!("bad upload: {err}")).into_response()
}

fn upload_error_response(err: &UploadError) -> Response {
    match err {
        UploadError::NotFileUpload => (StatusCode::BAD_REQUEST, "unsupported :action").into_response(),
        UploadError::Missing(field) => {
            (StatusCode::BAD_REQUEST, format!("missing required field: {field}")).into_response()
        }
        UploadError::DigestMismatch => (StatusCode::BAD_REQUEST, "sha256 digest mismatch").into_response(),
    }
}

/// `GET /api-docs/openapi.json` — the `OpenAPI` description of this server.
pub async fn openapi_spec() -> Response {
    static SPEC: std::sync::LazyLock<String> = std::sync::LazyLock::new(crate::api::openapi_json);
    ([(header::CONTENT_TYPE, "application/json")], SPEC.as_str()).into_response()
}

/// `GET /+status` — health, identity, counters, and the configured indexes. The web UI's live
/// dashboard refreshes from this document.
pub async fn status(State(state): State<Arc<AppState>>) -> Response {
    let serial = state.meta.current_serial().unwrap_or(0);
    let indexes: Vec<serde_json::Value> = state
        .describe_indexes()
        .into_iter()
        .map(|index| {
            serde_json::json!({
                "name": index.name,
                "route": index.route,
                "kind": index.kind,
                "layers": index.layers,
                "uploads": index.uploads,
            })
        })
        .collect();
    axum::Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "serial": serial,
        "requests": state.requests.load(Ordering::Relaxed),
        "metadata_requests": state.metadata_requests.load(Ordering::Relaxed),
        "indexes": indexes,
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
