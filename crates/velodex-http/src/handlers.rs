//! axum request handlers.
//!
//! All index traffic arrives on a catch-all path that is resolved to a configured index by longest
//! route prefix, then dispatched by method and remainder.
#![allow(
    clippy::result_large_err,
    reason = "handler helpers carry an axum Response as their error; boxing it everywhere adds noise"
)]

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::{Multipart, OriginalUri, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use velodex_core::pypi::{ProjectDetail, ProjectList, normalize_name, render_detail_html, render_index_html, to_json};
use velodex_storage::blob::Digest;

use crate::cache::{self, CacheError, PageOutcome};
use crate::metrics::Event;
use crate::path_safety::{self, PathSafetyError};
use crate::state::{AppState, Index, IndexKind};
use crate::upload::{self, UploadError, UploadForm};

const MIME_JSON: &str = "application/vnd.pypi.simple.v1+json";
const MIME_HTML: &str = "text/html; charset=utf-8";
const MEMBER_SIZE_HEADER: &str = "x-velodex-member-size";
const MEMBER_OFFSET_HEADER: &str = "x-velodex-member-offset";
const MEMBER_NEXT_OFFSET_HEADER: &str = "x-velodex-next-offset";

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
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    state.requests.fetch_add(1, Ordering::Relaxed);
    let path = uri.path().trim_start_matches('/');
    let Some((position, rest)) = state.resolve_position(path) else {
        return not_found();
    };
    let index = state.index_at(position);
    if rest == "simple/" {
        return index_response(cache::resolve_list(&state, index), negotiate(&headers));
    }
    if let Some(project) = rest.strip_prefix("simple/").and_then(|rest| rest.strip_suffix('/')) {
        let normalized = normalize_name(project);
        state.metrics.record(Event::Page {
            route: index.route.clone(),
            project: normalized.clone(),
        });
        if matches!(negotiate(&headers), Format::Json) {
            match cache::stream_detail(state.clone(), position, normalized.clone()).await {
                Ok(PageOutcome::Ready(bytes)) => {
                    return ([(header::CONTENT_TYPE, MIME_JSON), (header::VARY, "Accept")], bytes).into_response();
                }
                Ok(PageOutcome::Streaming(stream)) => {
                    return (
                        [(header::CONTENT_TYPE, MIME_JSON), (header::VARY, "Accept")],
                        axum::body::Body::from_stream(stream),
                    )
                        .into_response();
                }
                Ok(PageOutcome::NotFound) => {
                    return (StatusCode::NOT_FOUND, "project not found").into_response();
                }
                Ok(PageOutcome::Fallback) => {}
                Err(err) => {
                    tracing::error!(error = ?err, "streaming page failed, serving buffered");
                }
            }
        }
        let index = state.index_at(position);
        let detail = cache::resolve_detail(&state, index, &normalized, &index.route).await;
        return detail_response(detail, negotiate(&headers));
    }
    if let Some(file) = rest.strip_prefix("files/") {
        return file_route(&state, index.route.clone(), file).await;
    }
    if let Some(target) = rest.strip_prefix("inspect/") {
        return inspect_route(state.clone(), index.route.clone(), target, uri.query()).await;
    }
    not_found()
}

/// `GET /{route}/inspect/{sha256}/{filename}[/{member}]` — list a cached archive's members, or read
/// one member inline (pypi-browser-style introspection over the blob store).
async fn inspect_route(state: Arc<AppState>, route: String, target: &str, query: Option<&str>) -> Response {
    let Some((sha256, rest)) = target.split_once('/') else {
        return not_found();
    };
    let digest = match path_safety::parse_digest(sha256) {
        Ok(digest) => digest,
        Err(err) => return path_error_response(&err),
    };
    let archive_query = match archive_query(query) {
        Ok(query) => query,
        Err(response) => return response,
    };
    let (raw_filename, member) = match archive_query.member {
        Some(member) => (rest, Some(member)),
        None => match rest.split_once('/') {
            Some((filename, member)) => match path_safety::decode_path(member) {
                Ok(member) => (filename, Some(member)),
                Err(err) => return path_error_response(&err),
            },
            None => (rest, None),
        },
    };
    let filename = match safe_filename(raw_filename) {
        Ok(filename) => filename,
        Err(err) => return path_error_response(&err),
    };
    let path = match cache::file_path(state, digest, route, filename.clone()).await {
        Ok(path) => path,
        Err(err) => return file_response(Err(err)),
    };
    match member {
        Some(member) => archive_member(&filename, path, &member, archive_query.offset, archive_query.limit).await,
        None => archive_listing(&filename, path).await,
    }
}

struct ArchiveQuery {
    member: Option<String>,
    offset: u64,
    limit: u64,
}

fn archive_query(query: Option<&str>) -> Result<ArchiveQuery, Response> {
    let mut parsed = ArchiveQuery {
        member: None,
        offset: 0,
        limit: crate::archive::DEFAULT_MEMBER_CHUNK,
    };
    let Some(query) = query else {
        return Ok(parsed);
    };
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "member" => parsed.member = Some(value.into_owned()),
            "offset" => {
                parsed.offset = value
                    .parse::<u64>()
                    .map_err(|_| (StatusCode::BAD_REQUEST, "offset must be a non-negative integer").into_response())?;
            }
            "limit" => {
                let limit = value.parse::<u64>().map_err(|_| {
                    (
                        StatusCode::BAD_REQUEST,
                        "limit must be an integer between 1 and 1048576",
                    )
                        .into_response()
                })?;
                if !(1..=crate::archive::MAX_MEMBER_CHUNK).contains(&limit) {
                    return Err((
                        StatusCode::BAD_REQUEST,
                        format!("limit must be between 1 and {} bytes", crate::archive::MAX_MEMBER_CHUNK),
                    )
                        .into_response());
                }
                parsed.limit = limit;
            }
            _ => {}
        }
    }
    Ok(parsed)
}

/// Render an archive's member list as JSON.
async fn archive_listing(filename: &str, path: std::path::PathBuf) -> Response {
    let filename = filename.to_owned();
    match tokio::task::spawn_blocking({
        let filename = filename.clone();
        move || crate::archive::list_members_path(&filename, &path)
    })
    .await
    .expect("archive listing task panicked")
    {
        Ok(members) => axum::Json(serde_json::json!({ "filename": filename, "members": members })).into_response(),
        Err(err) => archive_error(&err, &filename, None),
    }
}

/// Serve one archive member chunk: UTF-8 chunks as text, anything else as bytes.
async fn archive_member(filename: &str, path: std::path::PathBuf, member: &str, offset: u64, limit: u64) -> Response {
    let filename = filename.to_owned();
    let member = member.to_owned();
    match tokio::task::spawn_blocking({
        let filename = filename.clone();
        let member = member.clone();
        move || crate::archive::read_member_chunk_path(&filename, &path, &member, offset, limit)
    })
    .await
    .expect("archive member task panicked")
    {
        Ok(chunk) => {
            let content_type = if std::str::from_utf8(&chunk.bytes).is_ok() {
                "text/plain; charset=utf-8"
            } else {
                "application/octet-stream"
            };
            let mut headers = HeaderMap::new();
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
            insert_header(&mut headers, MEMBER_SIZE_HEADER, chunk.size);
            insert_header(&mut headers, MEMBER_OFFSET_HEADER, chunk.offset);
            if let Some(next) = chunk.next_offset {
                insert_header(&mut headers, MEMBER_NEXT_OFFSET_HEADER, next);
            }
            (headers, chunk.bytes).into_response()
        }
        Err(err) => archive_error(&err, &filename, Some(&member)),
    }
}

fn insert_header(headers: &mut HeaderMap, name: &'static str, value: u64) {
    if let Ok(value) = HeaderValue::from_str(&value.to_string()) {
        headers.insert(name, value);
    }
}

fn archive_error(err: &crate::archive::ArchiveError, filename: &str, member: Option<&str>) -> Response {
    use crate::archive::ArchiveError;
    let status = match err {
        ArchiveError::Unsupported => StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ArchiveError::MemberNotFound => StatusCode::NOT_FOUND,
        ArchiveError::InvalidRange { .. } => StatusCode::RANGE_NOT_SATISFIABLE,
        ArchiveError::UnsafeMember(_) | ArchiveError::Read(_) => StatusCode::UNPROCESSABLE_ENTITY,
    };
    let target = member.map_or_else(
        || format!("archive {filename:?}"),
        |member| format!("member {member:?} in archive {filename:?}"),
    );
    (status, format!("{target}: {err}")).into_response()
}

async fn file_route(state: &Arc<AppState>, route: String, file: &str) -> Response {
    let Some((sha256, raw_filename)) = file.split_once('/') else {
        return not_found();
    };
    let digest = match path_safety::parse_digest(sha256) {
        Ok(digest) => digest,
        Err(err) => return path_error_response(&err),
    };
    let filename = match safe_filename(raw_filename) {
        Ok(filename) => filename,
        Err(err) => return path_error_response(&err),
    };
    if filename.ends_with(".metadata") {
        state.metadata_requests.fetch_add(1, Ordering::Relaxed);
        state.metrics.record(Event::Metadata { route, filename });
        return file_response(cache::metadata_bytes(state, &digest).await);
    }
    serve_blob(state, route, &filename, digest).await
}

fn safe_filename(raw: &str) -> Result<String, PathSafetyError> {
    let filename = path_safety::decode_path_segment(raw)?;
    path_safety::validate_filename(&filename)?;
    Ok(filename)
}

fn path_error_response(err: &PathSafetyError) -> Response {
    (StatusCode::BAD_REQUEST, err.to_string()).into_response()
}

/// Stream a blob to the client: from disk when cached, teed from the source mirror otherwise.
async fn serve_blob(state: &Arc<AppState>, route: String, filename: &str, digest: Digest) -> Response {
    let blob_headers = [
        (header::CONTENT_TYPE, "application/octet-stream"),
        (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
    ];
    match cache::stream_file(state.clone(), digest, route.clone(), filename.to_owned()).await {
        Ok(cache::FileOutcome::Cached(path)) => {
            let Ok(file) = tokio::fs::File::open(&path).await else {
                return (StatusCode::NOT_FOUND, "file not found").into_response();
            };
            let bytes = file.metadata().await.map(|meta| meta.len()).unwrap_or_default();
            state.metrics.record(Event::Download {
                route,
                filename: filename.to_owned(),
                bytes,
            });
            // The default 4 KiB buffer costs thousands of read syscalls on a wheel-sized blob; a
            // megabyte is the measured knee for both single and eight-way parallel hot reads.
            let stream = tokio_util::io::ReaderStream::with_capacity(file, 1024 * 1024);
            (blob_headers, axum::body::Body::from_stream(stream)).into_response()
        }
        // A live stream records its download event at EOF, when the byte count exists.
        Ok(cache::FileOutcome::Live(stream)) => (blob_headers, axum::body::Body::from_stream(stream)).into_response(),
        Err(CacheError::FileNotFound) => (StatusCode::NOT_FOUND, "file not found").into_response(),
        Err(err) => {
            tracing::error!(error = ?err, "file stream failed");
            (StatusCode::BAD_GATEWAY, "upstream error").into_response()
        }
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
        Ok(()) => {
            state.metrics.record(Event::Upload {
                route: index.route.clone(),
                project: prepared.normalized.clone(),
            });
            (StatusCode::OK, "upload accepted").into_response()
        }
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
            [(header::WWW_AUTHENTICATE, "Basic realm=\"velodex\"")],
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
/// as UTF-8 text. Unknown fields are ignored, as the upload API carries many metadata fields velodex
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
        UploadError::InvalidFilename(filename) => (
            StatusCode::BAD_REQUEST,
            format!(
                "invalid filename {filename:?}: filenames must be relative path segments without separators, traversal, or control characters"
            ),
        )
            .into_response(),
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
                "upload_to": index.upload_to,
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

/// The `/+stats` drill-down selectors.
#[derive(Debug, serde::Deserialize)]
pub struct StatsQuery {
    index: Option<String>,
    project: Option<String>,
}

/// `GET /+stats` — usage counters aggregated off-thread, drillable: no parameters for per-index
/// totals, `?index={route}` for its projects, `&project={name}` for its files.
pub async fn stats(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<StatsQuery>,
) -> Response {
    let tree = state.metrics.drill(query.index.as_deref(), query.project.as_deref());
    axum::Json(tree).into_response()
}

/// One per-index counter family: metric name, help text, and the counter it reads.
type CounterOf = fn(&crate::metrics::Counters) -> u64;

/// `GET /metrics` — Prometheus text exposition: the two global counters plus every per-index
/// counter the stats tree tracks, labelled by index route.
pub async fn metrics(State(state): State<Arc<AppState>>) -> Response {
    let requests = state.requests.load(Ordering::Relaxed);
    let metadata = state.metadata_requests.load(Ordering::Relaxed);
    let mut body = format!(
        "# HELP velodex_requests_total Total HTTP requests served.\n\
         # TYPE velodex_requests_total counter\n\
         velodex_requests_total {requests}\n\
         # HELP velodex_metadata_requests_total PEP 658 .metadata siblings served.\n\
         # TYPE velodex_metadata_requests_total counter\n\
         velodex_metadata_requests_total {metadata}\n"
    );
    let mut totals: Vec<_> = state.metrics.index_totals().into_iter().collect();
    totals.sort_by(|(a, _), (b, _)| a.cmp(b));
    let families: [(&str, &str, CounterOf); 10] = [
        ("velodex_index_pages_total", "Simple pages served.", |c| c.pages),
        ("velodex_index_downloads_total", "Artifacts served.", |c| c.downloads),
        ("velodex_index_download_bytes_total", "Artifact bytes served.", |c| {
            c.bytes
        }),
        ("velodex_index_metadata_total", "PEP 658 siblings served.", |c| {
            c.metadata
        }),
        ("velodex_index_uploads_total", "Distributions uploaded.", |c| c.uploads),
        ("velodex_index_refreshes_total", "Upstream revalidations.", |c| {
            c.refreshes
        }),
        (
            "velodex_index_pages_changed_total",
            "Revalidations that found upstream changed.",
            |c| c.changed,
        ),
        (
            "velodex_index_stale_served_total",
            "Pages served stale with upstream down.",
            |c| c.stale_served,
        ),
        (
            "velodex_index_upstream_errors_total",
            "Upstream failures with nothing cached.",
            |c| c.upstream_errors,
        ),
        (
            "velodex_index_rejected_total",
            "Downloads failing digest verification.",
            |c| c.rejected,
        ),
    ];
    for (name, help, value) in families {
        let _ = writeln!(body, "# HELP {name} {help}\n# TYPE {name} counter");
        for (route, counters) in &totals {
            let _ = writeln!(body, "{name}{{index=\"{route}\"}} {}", value(counters));
        }
    }
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], body).into_response()
}
