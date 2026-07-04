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

use axum::extract::{Multipart, OriginalUri, Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use blake2::Blake2bVar;
use blake2::digest::{Update as _, VariableOutput as _};
use velodex_format::pypi::{
    DistributionFilenameError, ProjectDetail, ProjectList, ProjectStatus, Yanked, normalize_name, render_detail_html,
    render_index_html, render_legacy_json, to_json,
};
use velodex_storage::blob::Digest;

use crate::cache::{self, CacheError, PageOutcome};
use crate::discovery::{self, BaseUrl};
use crate::metrics::Event;
use crate::path_safety::{self, PathSafetyError};
use crate::policy::{PolicyAction, PolicyDenial};
use crate::search::{SearchError, SearchParams};
use crate::state::{AppState, Index, IndexKind, describe_index};
use crate::upload::{self, StagedUpload, UploadError, UploadForm};
use crate::webhook::{WebhookEvent, WebhookEventKind};

const MIME_JSON: &str = "application/vnd.pypi.simple.v1+json";
const MIME_LEGACY_JSON: &str = "application/json";
const MIME_HTML: &str = "text/html; charset=utf-8";
const MEMBER_SIZE_HEADER: &str = "x-velodex-member-size";
const MEMBER_OFFSET_HEADER: &str = "x-velodex-member-offset";
const MEMBER_NEXT_OFFSET_HEADER: &str = "x-velodex-next-offset";
const MAX_UPLOAD_TEXT_FIELD_BYTES: usize = 64 * 1024;
const STATUS_RECENT_UPLOADS: usize = 5;

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
    if matches!(rest, "+api" | "+api/") {
        let base = BaseUrl::from_request(&headers, &uri);
        return index_api(&state, position, base.as_ref());
    }
    if matches!(rest, "+search" | "+search/") {
        let mut params = match SearchParams::from_query(uri.query()) {
            Ok(params) => params,
            Err(err) => return search_error_response(&err),
        };
        params.route = Some(index.route.clone());
        return search_response(&state, params);
    }
    match legacy_json_target(rest) {
        Ok(Some(target)) => {
            state.metrics.record(Event::Page {
                route: index.route.clone(),
                project: target.project.clone(),
            });
            return legacy_json_response(
                cache::resolve_detail(&state, index, &target.project, &index.route).await,
                &index.route,
                &target.project,
                target.version.as_deref(),
            );
        }
        Ok(None) => {}
        Err(response) => return response,
    }
    if rest == "simple/" {
        return index_response(cache::resolve_list(&state, index), negotiate(&headers), &index.route);
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
                Err(err @ CacheError::Simple(_)) => {
                    return detail_response(Err(err), Format::Json, &index.route, &normalized);
                }
                Err(err) => {
                    tracing::error!(error = ?err, "streaming page failed, serving buffered");
                }
            }
        }
        let index = state.index_at(position);
        let detail = cache::resolve_detail(&state, index, &normalized, &index.route).await;
        return detail_response(detail, negotiate(&headers), &index.route, &normalized);
    }
    if let Some(file) = rest.strip_prefix("files/") {
        return file_route(&state, index, file).await;
    }
    if let Some(target) = rest.strip_prefix("inspect/") {
        return inspect_route(state.clone(), index.route.clone(), target, uri.query()).await;
    }
    not_found()
}

/// `GET /{route}/inspect/{sha256}/{filename}` — list a cached archive's members, or read one text
/// member inline. Repeated `container` query parameters select nested archives.
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
        None if archive_query.containers.is_empty() => match rest.split_once('/') {
            Some((filename, member)) => match path_safety::decode_path(member) {
                Ok(member) => (filename, Some(member)),
                Err(err) => return path_error_response(&err),
            },
            None => (rest, None),
        },
        None => (rest, None),
    };
    let filename = match safe_filename(raw_filename) {
        Ok(filename) => filename,
        Err(err) => return path_error_response(&err),
    };
    let path = match cache::file_path(state, digest, route.clone(), filename.clone()).await {
        Ok(path) => path,
        Err(err) => {
            return file_response(Err(err), CacheContext::file(&route, sha256, &filename));
        }
    };
    match member {
        Some(member) => {
            archive_member(
                &filename,
                path,
                archive_query.containers,
                &member,
                archive_query.offset,
                archive_query.limit,
            )
            .await
        }
        None => archive_listing(&filename, path, archive_query.containers).await,
    }
}

struct ArchiveQuery {
    member: Option<String>,
    containers: Vec<String>,
    offset: u64,
    limit: u64,
}

fn archive_query(query: Option<&str>) -> Result<ArchiveQuery, Response> {
    let mut parsed = ArchiveQuery {
        member: None,
        containers: Vec::new(),
        offset: 0,
        limit: crate::archive::DEFAULT_MEMBER_CHUNK,
    };
    let Some(query) = query else {
        return Ok(parsed);
    };
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "member" => parsed.member = Some(value.into_owned()),
            "container" => parsed.containers.push(value.into_owned()),
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
async fn archive_listing(filename: &str, path: std::path::PathBuf, containers: Vec<String>) -> Response {
    let filename = filename.to_owned();
    match tokio::task::spawn_blocking({
        let filename = filename.clone();
        move || crate::archive::list_members_nested_path(&filename, &path, &containers)
    })
    .await
    .expect("archive listing task panicked")
    {
        Ok(members) => axum::Json(serde_json::json!({ "filename": filename, "members": members })).into_response(),
        Err(err) => archive_error(&err, &filename, None),
    }
}

/// Serve one text archive member chunk.
async fn archive_member(
    filename: &str,
    path: std::path::PathBuf,
    containers: Vec<String>,
    member: &str,
    offset: u64,
    limit: u64,
) -> Response {
    let filename = filename.to_owned();
    let member = member.to_owned();
    match tokio::task::spawn_blocking({
        let filename = filename.clone();
        let member = member.clone();
        move || {
            crate::archive::read_text_member_chunk_nested_path(&filename, &path, &containers, &member, offset, limit)
        }
    })
    .await
    .expect("archive member task panicked")
    {
        Ok(chunk) => {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            );
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
        ArchiveError::Unsupported | ArchiveError::UnsupportedNestedArchive(_) | ArchiveError::BinaryMember(_) => {
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        }
        ArchiveError::MemberNotFound => StatusCode::NOT_FOUND,
        ArchiveError::InvalidRange { .. } => StatusCode::RANGE_NOT_SATISFIABLE,
        ArchiveError::UnsafeMember(_)
        | ArchiveError::InvalidWheel(_)
        | ArchiveError::InvalidSdist(_)
        | ArchiveError::Read(_) => StatusCode::UNPROCESSABLE_ENTITY,
        ArchiveError::NestingTooDeep { .. } => StatusCode::BAD_REQUEST,
        ArchiveError::NestedArchiveTooLarge { .. } | ArchiveError::TooManyEntries(_) => StatusCode::PAYLOAD_TOO_LARGE,
    };
    let target = member.map_or_else(
        || format!("archive {filename:?}"),
        |member| format!("member {member:?} in archive {filename:?}"),
    );
    (status, format!("{target}: {err}")).into_response()
}

async fn file_route(state: &Arc<AppState>, index: &Index, file: &str) -> Response {
    let route = index.route.clone();
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
    if let Some(response) = download_policy_response(state, index, &filename, &digest) {
        return response;
    }
    match cache::download_status(state, index, &filename) {
        Ok(status) if !status.offers_downloads() => {
            return (
                StatusCode::FORBIDDEN,
                format!(
                    "project for file {filename:?} is {}; downloads are disabled",
                    status.marker()
                ),
            )
                .into_response();
        }
        Ok(_) => {}
        Err(err) => {
            return cache_error_response(&err, CacheContext::file(&route, digest.as_str(), &filename));
        }
    }
    if filename.ends_with(".metadata") {
        state.metadata_requests.fetch_add(1, Ordering::Relaxed);
        let digest_hex = digest.as_str().to_owned();
        state.metrics.record(Event::Metadata {
            route: route.clone(),
            filename: filename.clone(),
        });
        return file_response(
            cache::metadata_bytes(state, &digest, &route, &filename).await,
            CacheContext::metadata(&route, &digest_hex, &filename),
        );
    }
    serve_blob(state, route, &filename, digest).await
}

fn download_policy_response(state: &AppState, index: &Index, filename: &str, digest: &Digest) -> Option<Response> {
    let size = if state.blobs.exists(digest) {
        std::fs::metadata(state.blobs.path_for(digest))
            .ok()
            .map(|metadata| metadata.len())
    } else {
        cache::registered_file_size(state, digest).ok().flatten()
    };
    index
        .policy
        .check_download(PolicyAction::Serve, filename, size)
        .err()
        .map(|denial| policy_denial_response(&denial))
}

fn safe_filename(raw: &str) -> Result<String, PathSafetyError> {
    let filename = path_safety::decode_path_segment(raw)?;
    path_safety::validate_filename(&filename)?;
    Ok(filename)
}

fn path_error_response(err: &PathSafetyError) -> Response {
    (StatusCode::BAD_REQUEST, err.to_string()).into_response()
}

struct LegacyJsonTarget {
    project: String,
    version: Option<String>,
}

fn legacy_json_target(rest: &str) -> Result<Option<LegacyJsonTarget>, Response> {
    let trimmed = rest.trim_end_matches('/');
    let Some(spec) = trimmed.strip_suffix("/json") else {
        return Ok(None);
    };
    let Some((project, version)) = spec.split_once('/') else {
        let project = path_safety::decode_path_segment(spec).map_err(|err| path_error_response(&err))?;
        path_safety::validate_path_segment("project", &project).map_err(|err| path_error_response(&err))?;
        return Ok(Some(LegacyJsonTarget {
            project: normalize_name(&project),
            version: None,
        }));
    };
    let project = path_safety::decode_path_segment(project).map_err(|err| path_error_response(&err))?;
    let version = path_safety::decode_path(version).map_err(|err| path_error_response(&err))?;
    path_safety::validate_path_segment("project", &project).map_err(|err| path_error_response(&err))?;
    path_safety::validate_path_segment("version", &version).map_err(|err| path_error_response(&err))?;
    Ok(Some(LegacyJsonTarget {
        project: normalize_name(&project),
        version: Some(version),
    }))
}

/// Stream a blob to the client: from disk when cached, teed from the source mirror otherwise.
async fn serve_blob(state: &Arc<AppState>, route: String, filename: &str, digest: Digest) -> Response {
    let digest_hex = digest.as_str().to_owned();
    let blob_headers = [
        (header::CONTENT_TYPE, "application/octet-stream"),
        (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
    ];
    match cache::stream_file(state.clone(), digest, route.clone(), filename.to_owned()).await {
        Ok(cache::FileOutcome::Cached(path)) => {
            let Ok(file) = tokio::fs::File::open(&path).await else {
                return (
                    StatusCode::NOT_FOUND,
                    format!("cached file missing on index {route:?}: digest {digest_hex}, filename {filename:?}"),
                )
                    .into_response();
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
        Err(err) => {
            tracing::error!(error = ?err, "file stream failed");
            cache_error_response(&err, CacheContext::file(&route, &digest_hex, filename))
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
    let actor = crate::security::actor(&headers);
    let Some((index, rest)) = state.resolve(&path) else {
        return not_found();
    };
    if !rest.is_empty() {
        security_upload_event(&headers, actor.as_deref(), &index.route, None, "denied")
            .reason(Some("upload path must target an index root"))
            .emit();
        return not_found();
    }
    let Some(local) = upload_target(&state, index) else {
        security_upload_event(&headers, actor.as_deref(), &index.route, None, "denied")
            .reason(Some("index does not accept uploads"))
            .emit();
        return (StatusCode::METHOD_NOT_ALLOWED, "index does not accept uploads").into_response();
    };
    if let Err(response) = authorize(local, &headers) {
        return response;
    }
    accept_upload(&state, index, local, &headers, actor.as_deref(), multipart).await
}

async fn accept_upload(
    state: &Arc<AppState>,
    index: &Index,
    local: &Index,
    headers: &HeaderMap,
    actor: Option<&str>,
    multipart: Multipart,
) -> Response {
    let (form, staged) = match collect_form(multipart, &state.blobs).await {
        Ok(form) => form,
        Err(response) => {
            security_upload_event(headers, actor, &index.route, Some(&local.name), "failure")
                .reason(Some("multipart body rejected"))
                .emit();
            return response;
        }
    };
    let Some(staged) = staged else {
        let err = UploadError::Missing("content");
        let (_, reason) = upload_error_message(&err);
        security_upload_event(headers, actor, &index.route, Some(&local.name), "denied")
            .project(form.name.as_deref().map(normalize_name).as_deref())
            .version(form.version.as_deref())
            .reason(Some(&reason))
            .emit();
        return upload_error_response(&err);
    };
    let form_project = form.name.as_deref().map(normalize_name);
    let form_version = form.version.clone();
    let form_filename = form.filename.clone();
    let upload_time_unix = (state.clock)();
    let prepared = match upload::prepare(form, staged, &index.route, upload_time_unix) {
        Ok(prepared) => prepared,
        Err(err) => {
            let (_, reason) = upload_error_message(&err);
            security_upload_event(headers, actor, &index.route, Some(&local.name), "denied")
                .project(form_project.as_deref())
                .version(form_version.as_deref())
                .filename(form_filename.as_deref())
                .reason(Some(&reason))
                .emit();
            return upload_error_response(&err);
        }
    };
    let project = prepared.normalized.clone();
    let version = prepared.record.version.clone();
    let filename = prepared.filename.clone();
    let digest = prepared.digest.as_str().to_owned();
    let audit = UploadAudit {
        headers,
        actor: actor.map(str::to_owned),
        request_id: request_id(headers),
        created_at_unix: upload_time_unix,
        index: &index.name,
        route: &index.route,
        local: &local.name,
        project: &project,
        version: &version,
        filename: &filename,
        digest: &digest,
    };
    if let Some(block) = upload_policy_response(index, &prepared, &audit) {
        return block;
    }
    if local.name != index.name
        && let Some(block) = upload_policy_response(local, &prepared, &audit)
    {
        return block;
    }
    if let Some(block) = upload_status_response(
        cache::project_status(state, index, &project).await,
        &index.route,
        &project,
    ) {
        emit_upload_status_event(&audit, &block);
        return block.response;
    }
    upload_store_response(state, &audit, cache::store_upload(state, &local.name, prepared))
}

fn upload_policy_response(
    index: &Index,
    prepared: &upload::PreparedUpload,
    audit: &UploadAudit<'_>,
) -> Option<Response> {
    index
        .policy
        .check_file(PolicyAction::Upload, &prepared.normalized, &prepared.record.file)
        .err()
        .map(|denial| {
            security_upload_event(
                audit.headers,
                audit.actor.as_deref(),
                audit.route,
                Some(audit.local),
                "denied",
            )
            .project(Some(audit.project))
            .version(Some(audit.version))
            .filename(Some(audit.filename))
            .digest(Some(audit.digest))
            .reason(Some(&denial.reason))
            .emit();
            policy_denial_response(&denial)
        })
}

struct UploadAudit<'a> {
    headers: &'a HeaderMap,
    actor: Option<String>,
    request_id: Option<String>,
    created_at_unix: i64,
    index: &'a str,
    route: &'a str,
    local: &'a str,
    project: &'a str,
    version: &'a str,
    filename: &'a str,
    digest: &'a str,
}

fn upload_store_response(state: &Arc<AppState>, audit: &UploadAudit<'_>, result: Result<bool, CacheError>) -> Response {
    match result {
        Ok(stored) => {
            if stored {
                state.metrics.record(Event::Upload {
                    route: audit.route.to_owned(),
                    project: audit.project.to_owned(),
                });
                crate::webhook::emit(
                    state.clone(),
                    &WebhookEvent {
                        kind: WebhookEventKind::Upload,
                        created_at_unix: audit.created_at_unix,
                        index: audit.index.to_owned(),
                        route: audit.route.to_owned(),
                        local_index: audit.local.to_owned(),
                        project: audit.project.to_owned(),
                        version: Some(audit.version.to_owned()),
                        filename: Some(audit.filename.to_owned()),
                        digest: Some(audit.digest.to_owned()),
                        count: 1,
                        actor: audit.actor.clone(),
                        request_id: audit.request_id.clone(),
                    },
                );
            }
            security_upload_event(
                audit.headers,
                audit.actor.as_deref(),
                audit.route,
                Some(audit.local),
                if stored { "success" } else { "noop" },
            )
            .project(Some(audit.project))
            .version(Some(audit.version))
            .filename(Some(audit.filename))
            .digest(Some(audit.digest))
            .count(usize::from(stored))
            .reason((!stored).then_some("same content already stored"))
            .emit();
            (StatusCode::OK, "upload accepted").into_response()
        }
        Err(CacheError::FileExists(filename)) => {
            security_upload_event(
                audit.headers,
                audit.actor.as_deref(),
                audit.route,
                Some(audit.local),
                "denied",
            )
            .project(Some(audit.project))
            .version(Some(audit.version))
            .filename(Some(&filename))
            .digest(Some(audit.digest))
            .reason(Some("file exists with different content"))
            .emit();
            (
                StatusCode::BAD_REQUEST,
                format!("File already exists: {filename:?} has different content; use a different filename"),
            )
                .into_response()
        }
        Err(err) => {
            let reason = err.user_message();
            security_upload_event(
                audit.headers,
                audit.actor.as_deref(),
                audit.route,
                Some(audit.local),
                "failure",
            )
            .project(Some(audit.project))
            .version(Some(audit.version))
            .filename(Some(audit.filename))
            .digest(Some(audit.digest))
            .reason(Some(&reason))
            .emit();
            tracing::error!(error = ?err, "upload store failed");
            cache_error_response(&err, CacheContext::upload(audit.route, audit.project))
        }
    }
}

fn emit_upload_status_event(audit: &UploadAudit<'_>, block: &UploadStatusBlock) {
    security_upload_event(
        audit.headers,
        audit.actor.as_deref(),
        audit.route,
        Some(audit.local),
        block.result,
    )
    .project(Some(audit.project))
    .version(Some(audit.version))
    .filename(Some(audit.filename))
    .digest(Some(audit.digest))
    .reason(Some(&block.reason))
    .emit();
}

#[derive(Clone, Copy)]
struct PromotionAudit<'a> {
    headers: &'a HeaderMap,
    actor: Option<&'a str>,
    repository: &'a str,
    source_repository: &'a str,
    local_repository: &'a str,
    project: &'a str,
    version: &'a str,
}

fn emit_promotion_status_event(audit: &PromotionAudit<'_>, block: &UploadStatusBlock) {
    crate::security::Event::new("promote", block.result)
        .actor(audit.actor)
        .repository(audit.repository)
        .source_repository(audit.source_repository)
        .local_repository(audit.local_repository)
        .project(Some(audit.project))
        .version(Some(audit.version))
        .reason(Some(&block.reason))
        .request(audit.headers)
        .emit();
}

struct UploadStatusBlock {
    response: Response,
    result: &'static str,
    reason: String,
}

fn upload_status_response(
    result: Result<ProjectStatus, CacheError>,
    index: &str,
    project: &str,
) -> Option<UploadStatusBlock> {
    match result {
        Ok(status) if status.allows_uploads() => None,
        Ok(status) => {
            let reason = format!("project {project:?} is {}; uploads are disabled", status.marker());
            Some(UploadStatusBlock {
                response: (StatusCode::FORBIDDEN, reason.clone()).into_response(),
                result: "denied",
                reason,
            })
        }
        Err(err) => {
            let reason = err.user_message();
            Some(UploadStatusBlock {
                response: cache_error_response(&err, CacheContext::upload(index, project)),
                result: "failure",
                reason,
            })
        }
    }
}

fn promotion_source_route(query: Option<&str>) -> Result<String, Response> {
    let Some(query) = query else {
        return Err((StatusCode::BAD_REQUEST, "promotion requires from={source route}").into_response());
    };
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if key == "from" && !value.is_empty() {
            return Ok(value.into_owned());
        }
    }
    Err((StatusCode::BAD_REQUEST, "promotion requires from={source route}").into_response())
}

fn promotion_source<'a>(state: &'a AppState, route: &str) -> Result<&'a Index, Response> {
    let route = route.trim_matches('/');
    state
        .indexes
        .iter()
        .find(|index| index.route == route)
        .ok_or_else(not_found)
}

/// `PUT /{route}/{project}/[{version}/]yank` marks files yanked (PEP 592, reversible);
/// `PUT .../restore` clears the hidden marker a DELETE left on read-only upstream files.
pub async fn dispatch_put(
    State(state): State<Arc<AppState>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    state.requests.fetch_add(1, Ordering::Relaxed);
    let path = uri.path().trim_start_matches('/');
    let actor = crate::security::actor(&headers);
    let (index, local, spec) = match removal_target(&state, path, &headers) {
        Ok(target) => target,
        Err(response) => return response,
    };
    if let Some(spec) = strip_action_segment(spec, "promote") {
        return promote_request(&state, index, local, spec, uri.query(), &headers, actor.as_deref()).await;
    }
    if let Some(spec) = strip_action_segment(spec, "yank") {
        return yank_request(&state, index, local, spec, uri.query(), &headers, actor.as_deref()).await;
    }
    if let Some(spec) = strip_action_segment(spec, "restore") {
        return restore_request(&state, index, local, spec, &headers, actor.as_deref());
    }
    not_found()
}

async fn promote_request(
    state: &Arc<AppState>,
    index: &Index,
    local: &Index,
    spec: &str,
    query: Option<&str>,
    headers: &HeaderMap,
    actor: Option<&str>,
) -> Response {
    let source_route = match promotion_source_route(query) {
        Ok(route) => route,
        Err(response) => return response,
    };
    let source = match promotion_source(state, &source_route) {
        Ok(source) => source,
        Err(response) => return response,
    };
    let Some(source_local) = upload_target(state, source) else {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            format!("source index {source_route:?} has no local upload layer"),
        )
            .into_response();
    };
    let (project, version) = match parse_project_version(spec) {
        Ok((project, Some(version))) => (project, version),
        Ok((_project, None)) => return (StatusCode::BAD_REQUEST, "promotion requires a version").into_response(),
        Err(response) => return response,
    };
    let audit = PromotionAudit {
        headers,
        actor,
        repository: &index.route,
        source_repository: &source.route,
        local_repository: &local.name,
        project: &project,
        version: &version,
    };
    if let Some(block) = upload_status_response(
        cache::project_status(state, index, &project).await,
        &index.route,
        &project,
    ) {
        emit_promotion_status_event(&audit, &block);
        return block.response;
    }
    let result = cache::promote_release(state, &source_local.name, &local.name, &index.route, &project, &version);
    security_promotion_event(audit, &result);
    promotion_response(result)
}

async fn yank_request(
    state: &Arc<AppState>,
    index: &Index,
    local: &Index,
    spec: &str,
    query: Option<&str>,
    headers: &HeaderMap,
    actor: Option<&str>,
) -> Response {
    let (project, version) = match parse_project_version(spec) {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let result = cache::set_yanked(
        state,
        index,
        &local.name,
        &project,
        version.as_deref(),
        yank_marker(query),
    )
    .await;
    let audit = MutationAudit {
        headers,
        action: "yank",
        actor,
        index: &index.name,
        repository: &index.route,
        local_repository: &local.name,
        project: &project,
        version: version.as_deref(),
        request_id: request_id(headers),
    };
    security_mutation_event(&audit, &result);
    emit_mutation_webhook(state.clone(), WebhookEventKind::Yank, &audit, &result);
    count_response(result)
}

fn restore_request(
    state: &Arc<AppState>,
    index: &Index,
    local: &Index,
    spec: &str,
    headers: &HeaderMap,
    actor: Option<&str>,
) -> Response {
    let (project, version) = match parse_project_version(spec) {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let result = cache::restore_files(state, &local.name, &project, version.as_deref());
    let audit = MutationAudit {
        headers,
        action: "restore",
        actor,
        index: &index.name,
        repository: &index.route,
        local_repository: &local.name,
        project: &project,
        version: version.as_deref(),
        request_id: request_id(headers),
    };
    security_mutation_event(&audit, &result);
    emit_mutation_webhook(state.clone(), WebhookEventKind::Restore, &audit, &result);
    count_response(result)
}

/// `DELETE /{route}/{project}/[{version}/]` removes files: uploads are deleted outright (volatile
/// indexes only), read-only upstream files are hidden reversibly. A `.../yank` suffix un-yanks.
pub async fn dispatch_delete(
    State(state): State<Arc<AppState>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    state.requests.fetch_add(1, Ordering::Relaxed);
    let path = uri.path().trim_start_matches('/');
    let actor = crate::security::actor(&headers);
    let (index, local, spec) = match removal_target(&state, path, &headers) {
        Ok(target) => target,
        Err(response) => return response,
    };
    if let Some(spec) = strip_action_segment(spec, "yank") {
        let (project, version) = match parse_project_version(spec) {
            Ok(parsed) => parsed,
            Err(response) => return response,
        };
        let result = cache::set_yanked(&state, index, &local.name, &project, version.as_deref(), Yanked::No).await;
        let audit = MutationAudit {
            headers: &headers,
            action: "unyank",
            actor: actor.as_deref(),
            index: &index.name,
            repository: &index.route,
            local_repository: &local.name,
            project: &project,
            version: version.as_deref(),
            request_id: request_id(&headers),
        };
        security_mutation_event(&audit, &result);
        emit_mutation_webhook(state.clone(), WebhookEventKind::Unyank, &audit, &result);
        return count_response(result);
    }
    let (project, version) = match parse_project_version(spec) {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let volatile = is_volatile(local);
    let result = cache::remove_files(&state, index, &local.name, volatile, &project, version.as_deref()).await;
    let audit = MutationAudit {
        headers: &headers,
        action: "delete",
        actor: actor.as_deref(),
        index: &index.name,
        repository: &index.route,
        local_repository: &local.name,
        project: &project,
        version: version.as_deref(),
        request_id: request_id(&headers),
    };
    security_mutation_event(&audit, &result);
    emit_mutation_webhook(state.clone(), WebhookEventKind::Delete, &audit, &result);
    count_response(result)
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
        IndexKind::Mirror { .. } | IndexKind::Overlay { upload: None, .. } => None,
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
    let actor = crate::security::actor(headers);
    let Some(token) = upload_token.as_deref() else {
        security_token_event(headers, actor.as_deref(), &local.name, "denied", "uploads are disabled");
        return Err((StatusCode::FORBIDDEN, "uploads are disabled").into_response());
    };
    let auth = headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok());
    if upload::authorized(auth, token) {
        security_token_event(headers, actor.as_deref(), &local.name, "success", "");
        Ok(())
    } else {
        security_token_event(headers, actor.as_deref(), &local.name, "denied", "invalid upload token");
        Err((
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"velodex\"")],
            "unauthorized",
        )
            .into_response())
    }
}

fn request_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn security_upload_event<'a>(
    headers: &'a HeaderMap,
    actor: Option<&'a str>,
    repository: &'a str,
    local_repository: Option<&'a str>,
    result: &'static str,
) -> crate::security::Event<'a> {
    let event = crate::security::Event::new("upload", result)
        .actor(actor)
        .repository(repository)
        .request(headers);
    if let Some(local_repository) = local_repository {
        event.local_repository(local_repository)
    } else {
        event
    }
}

fn security_token_event(
    headers: &HeaderMap,
    actor: Option<&str>,
    repository: &str,
    result: &'static str,
    reason: &str,
) {
    let event = crate::security::Event::new("token_use", result)
        .actor(actor)
        .repository(repository)
        .request(headers);
    if reason.is_empty() {
        event.emit();
    } else {
        event.reason(Some(reason)).emit();
    }
}

struct MutationAudit<'a> {
    headers: &'a HeaderMap,
    action: &'static str,
    actor: Option<&'a str>,
    index: &'a str,
    repository: &'a str,
    local_repository: &'a str,
    project: &'a str,
    version: Option<&'a str>,
    request_id: Option<String>,
}

fn security_mutation_event(audit: &MutationAudit<'_>, result: &Result<usize, CacheError>) {
    let (security_result, count, reason) = match result {
        Ok(0) => ("noop", 0, Some("nothing matched".to_owned())),
        Ok(count) => ("success", *count, None),
        Err(CacheError::NotVolatile) => ("denied", 0, Some(CacheError::NotVolatile.user_message())),
        Err(err) => ("failure", 0, Some(err.user_message())),
    };
    crate::security::Event::new(audit.action, security_result)
        .actor(audit.actor)
        .repository(audit.repository)
        .local_repository(audit.local_repository)
        .project(Some(audit.project))
        .version(audit.version)
        .count(count)
        .reason(reason.as_deref())
        .request(audit.headers)
        .emit();
}

fn security_promotion_event(audit: PromotionAudit<'_>, result: &Result<usize, CacheError>) {
    let (security_result, count, reason) = match result {
        Ok(0) => ("noop", 0, Some("same files already exist on target".to_owned())),
        Ok(count) => ("success", *count, None),
        Err(err @ (CacheError::FileExists(_) | CacheError::NoPromotableFiles { .. })) => {
            ("denied", 0, Some(err.user_message()))
        }
        Err(err) => ("failure", 0, Some(err.user_message())),
    };
    crate::security::Event::new("promote", security_result)
        .actor(audit.actor)
        .repository(audit.repository)
        .source_repository(audit.source_repository)
        .local_repository(audit.local_repository)
        .project(Some(audit.project))
        .version(Some(audit.version))
        .count(count)
        .reason(reason.as_deref())
        .request(audit.headers)
        .emit();
}

fn emit_mutation_webhook(
    state: Arc<AppState>,
    kind: WebhookEventKind,
    audit: &MutationAudit<'_>,
    result: &Result<usize, CacheError>,
) {
    let Ok(count) = result else {
        return;
    };
    if *count == 0 {
        return;
    }
    let created_at_unix = (state.clock)();
    crate::webhook::emit(
        state,
        &WebhookEvent {
            kind,
            created_at_unix,
            index: audit.index.to_owned(),
            route: audit.repository.to_owned(),
            local_index: audit.local_repository.to_owned(),
            project: audit.project.to_owned(),
            version: audit.version.map(str::to_owned),
            filename: None,
            digest: None,
            count: *count,
            actor: audit.actor.map(str::to_owned),
            request_id: audit.request_id.clone(),
        },
    );
}

fn strip_action_segment<'a>(spec: &'a str, action: &str) -> Option<&'a str> {
    let spec = spec.trim_end_matches('/');
    let base = spec.strip_suffix(action)?;
    (base.is_empty() || base.ends_with('/')).then_some(base)
}

fn parse_project_version(spec: &str) -> Result<(String, Option<String>), Response> {
    let trimmed = spec.trim_matches('/');
    let mut parts = trimmed.splitn(2, '/');
    let project = parts
        .next()
        .map(path_safety::decode_path_segment)
        .transpose()
        .map_err(|err| path_error_response(&err))?
        .unwrap_or_default();
    path_safety::validate_path_segment("project", &project).map_err(|err| path_error_response(&err))?;
    let version = parts
        .next()
        .map(|version| path_safety::decode_path(version.trim_matches('/')))
        .transpose()
        .map_err(|err| path_error_response(&err))?
        .filter(|version| !version.is_empty());
    if let Some(version) = &version {
        path_safety::validate_path_segment("version", version).map_err(|err| path_error_response(&err))?;
    }
    Ok((normalize_name(&project), version))
}

fn yank_marker(query: Option<&str>) -> Yanked {
    let Some(query) = query else {
        return Yanked::Yes;
    };
    let mut reason = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if key != "reason" {
            continue;
        }
        reason = Some(value.into_owned());
    }
    reason
        .filter(|reason| !reason.is_empty())
        .map_or(Yanked::Yes, Yanked::Reason)
}

/// Map a project-list result to a negotiated response. Sync so every arm is directly testable.
pub(crate) fn index_response(result: Result<ProjectList, CacheError>, format: Format, index: &str) -> Response {
    let list = match result {
        Ok(list) => list,
        Err(err) => return cache_error_response(&err, CacheContext::list(index)),
    };
    let vary = (header::VARY, "Accept");
    match format {
        Format::Json => ([(header::CONTENT_TYPE, MIME_JSON), vary], to_json(&list)).into_response(),
        Format::Html => ([(header::CONTENT_TYPE, MIME_HTML), vary], render_index_html(&list)).into_response(),
    }
}

/// Map a resolved project detail to a negotiated response. Kept sync so every arm is directly
/// unit-testable.
pub(crate) fn detail_response(
    result: Result<Option<ProjectDetail>, CacheError>,
    format: Format,
    index: &str,
    project: &str,
) -> Response {
    let detail = match result {
        Ok(Some(detail)) => detail,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                format!("project {project:?} was not found on index {index:?}"),
            )
                .into_response();
        }
        Err(CacheError::Upstream(
            err @ (velodex_upstream::UpstreamError::MissingContentType { .. }
            | velodex_upstream::UpstreamError::UnsupportedContentType { .. }),
        )) => {
            tracing::warn!(error = ?err, "unsupported upstream simple api content type");
            return (StatusCode::BAD_GATEWAY, err.to_string()).into_response();
        }
        Err(err) => {
            tracing::error!(error = ?err, "project detail failed");
            return cache_error_response(&err, CacheContext::project(index, project));
        }
    };
    let vary = (header::VARY, "Accept");
    match format {
        Format::Json => ([(header::CONTENT_TYPE, MIME_JSON), vary], to_json(&detail)).into_response(),
        Format::Html => ([(header::CONTENT_TYPE, MIME_HTML), vary], render_detail_html(&detail)).into_response(),
    }
}

fn legacy_json_response(
    result: Result<Option<ProjectDetail>, CacheError>,
    index: &str,
    project: &str,
    version: Option<&str>,
) -> Response {
    let detail = match result {
        Ok(Some(detail)) => detail,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                format!("project {project:?} was not found on index {index:?}"),
            )
                .into_response();
        }
        Err(CacheError::Upstream(
            err @ (velodex_upstream::UpstreamError::MissingContentType { .. }
            | velodex_upstream::UpstreamError::UnsupportedContentType { .. }),
        )) => {
            tracing::warn!(error = ?err, "unsupported upstream simple api content type");
            return (StatusCode::BAD_GATEWAY, err.to_string()).into_response();
        }
        Err(err) => {
            tracing::error!(error = ?err, "legacy project json failed");
            return cache_error_response(&err, CacheContext::project(index, project));
        }
    };
    let Some(body) = render_legacy_json(&detail, version) else {
        return (
            StatusCode::NOT_FOUND,
            format!("version {version:?} was not found for project {project:?} on index {index:?}"),
        )
            .into_response();
    };
    ([(header::CONTENT_TYPE, MIME_LEGACY_JSON)], body).into_response()
}

/// Map a file-bytes result to a response. Sync so every arm is directly unit-testable.
pub(crate) fn file_response(result: Result<bytes::Bytes, CacheError>, context: CacheContext<'_>) -> Response {
    match result {
        Ok(body) => (
            [
                (header::CONTENT_TYPE, "application/octet-stream"),
                (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
            ],
            body,
        )
            .into_response(),
        Err(err) => cache_error_response(&err, context),
    }
}

fn count_response(result: Result<usize, CacheError>) -> Response {
    match result {
        Ok(0) => (StatusCode::NOT_FOUND, "nothing to remove").into_response(),
        Ok(count) => (StatusCode::OK, format!("affected {count} file(s)")).into_response(),
        Err(err) => {
            tracing::error!(error = ?err, "removal failed");
            cache_error_response(&err, CacheContext::mutation("file removal"))
        }
    }
}

fn promotion_response(result: Result<usize, CacheError>) -> Response {
    match result {
        Ok(count) => (StatusCode::OK, format!("promoted {count} file(s)")).into_response(),
        Err(CacheError::FileExists(filename)) => (
            StatusCode::CONFLICT,
            format!("File already exists: {filename:?} has different content; use a different filename"),
        )
            .into_response(),
        Err(err) => {
            tracing::error!(error = ?err, "promotion failed");
            cache_error_response(&err, CacheContext::mutation("promotion"))
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct CacheContext<'a> {
    operation: &'static str,
    index: Option<&'a str>,
    project: Option<&'a str>,
    digest: Option<&'a str>,
    filename: Option<&'a str>,
}

impl<'a> CacheContext<'a> {
    const fn list(index: &'a str) -> Self {
        Self {
            operation: "project list",
            index: Some(index),
            project: None,
            digest: None,
            filename: None,
        }
    }

    const fn project(index: &'a str, project: &'a str) -> Self {
        Self {
            operation: "project detail",
            index: Some(index),
            project: Some(project),
            digest: None,
            filename: None,
        }
    }

    const fn file(index: &'a str, digest: &'a str, filename: &'a str) -> Self {
        Self {
            operation: "file download",
            index: Some(index),
            project: None,
            digest: Some(digest),
            filename: Some(filename),
        }
    }

    const fn metadata(index: &'a str, digest: &'a str, filename: &'a str) -> Self {
        Self {
            operation: "metadata fetch",
            index: Some(index),
            project: None,
            digest: Some(digest),
            filename: Some(filename),
        }
    }

    const fn upload(index: &'a str, project: &'a str) -> Self {
        Self {
            operation: "upload storage",
            index: Some(index),
            project: Some(project),
            digest: None,
            filename: None,
        }
    }

    const fn mutation(operation: &'static str) -> Self {
        Self {
            operation,
            index: None,
            project: None,
            digest: None,
            filename: None,
        }
    }
}

fn cache_error_response(err: &CacheError, context: CacheContext<'_>) -> Response {
    if let CacheError::Policy(denial) = err {
        return policy_denial_response(denial);
    }
    if let CacheError::RateLimited { retry_after } = err {
        let mut response = (cache_error_status(err, &context), cache_error_message(err, context)).into_response();
        response.headers_mut().insert(
            header::RETRY_AFTER,
            HeaderValue::from_str(&retry_after.to_string()).expect("integer retry-after is a valid header"),
        );
        return response;
    }
    (cache_error_status(err, &context), cache_error_message(err, context)).into_response()
}

fn policy_denial_response(denial: &PolicyDenial) -> Response {
    (
        StatusCode::FORBIDDEN,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_vec(denial).expect("policy denial always serializes"),
    )
        .into_response()
}

fn cache_error_status(err: &CacheError, context: &CacheContext<'_>) -> StatusCode {
    match err {
        CacheError::Meta(_) | CacheError::Blob(_) | CacheError::MissingSha256(_) => StatusCode::INTERNAL_SERVER_ERROR,
        CacheError::FileNotFound | CacheError::NoPromotableFiles { .. } => StatusCode::NOT_FOUND,
        CacheError::OfflineMissing(_) => StatusCode::SERVICE_UNAVAILABLE,
        CacheError::FileExists(_) => StatusCode::CONFLICT,
        CacheError::NotVolatile | CacheError::Policy(_) => StatusCode::FORBIDDEN,
        CacheError::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
        CacheError::Parse(_) if matches!(context.operation, "upload storage" | "file removal" | "promotion") => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
        CacheError::Upstream(_)
        | CacheError::Archive(_)
        | CacheError::Parse(_)
        | CacheError::Simple(_)
        | CacheError::Unavailable
        | CacheError::Stream(_) => StatusCode::BAD_GATEWAY,
    }
}

fn cache_error_message(err: &CacheError, context: CacheContext<'_>) -> String {
    let mut message = context.operation.to_owned();
    if let Some(index) = context.index {
        let _ = write!(message, " on index {index:?}");
    }
    if let Some(project) = context.project {
        let _ = write!(message, " for project {project:?}");
    }
    if let Some(filename) = context.filename {
        let _ = write!(message, " for file {filename:?}");
    }
    if let Some(digest) = context.digest {
        let _ = write!(message, " with digest {digest}");
    }
    message.push_str(": ");
    message.push_str(&err.user_message());
    message
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}

fn search_response(state: &AppState, params: SearchParams) -> Response {
    match state.search.search(state, params) {
        Ok(results) => axum::Json(results).into_response(),
        Err(err) => search_error_response(&err),
    }
}

fn search_error_response(err: &SearchError) -> Response {
    let status = match err {
        SearchError::InvalidSource(_) | SearchError::Tantivy(tantivy::TantivyError::InvalidArgument(_)) => {
            StatusCode::BAD_REQUEST
        }
        SearchError::Tantivy(_)
        | SearchError::Directory(_)
        | SearchError::Io(_)
        | SearchError::Meta(_)
        | SearchError::Blob(_)
        | SearchError::Json(_)
        | SearchError::Simple(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, axum::Json(serde_json::json!({ "error": err.to_string() }))).into_response()
}

/// Drain a multipart body into an [`UploadForm`], staging the `content` part on disk while the rest
/// stays as UTF-8 text. Unknown fields are ignored, as the upload API carries many metadata fields
/// velodex does not need. Every read or decode error funnels through [`reject`] as a 400.
async fn collect_form(
    mut multipart: Multipart,
    blobs: &velodex_storage::blob::BlobStore,
) -> Result<(UploadForm, Option<StagedUpload>), Response> {
    let mut form = UploadForm::default();
    let mut staged = None;
    while let Some(field) = multipart.next_field().await.map_err(reject)? {
        let field_name = field.name().unwrap_or_default().to_owned();
        if field_name == "content" {
            if staged.is_some() {
                return Err(reject("duplicate content field"));
            }
            form.filename = field.file_name().map(str::to_owned);
            staged = Some(stage_content(field, blobs).await?);
        } else if let Some(upload_field) = upload_text_field(&field_name) {
            let value = read_text_field(field, &field_name).await?;
            set_upload_text_field(&mut form, upload_field, value);
        } else {
            drain_field(field).await?;
        }
    }
    Ok((form, staged))
}

#[derive(Clone, Copy)]
enum UploadTextField {
    Action,
    MetadataVersion,
    Name,
    Version,
    RequiresPython,
    License,
    LicenseExpression,
    LicenseFile,
    ProvidesExtra,
    ProjectUrl,
    HomePage,
    Filetype,
    Sha256Digest,
    Blake2Digest,
    Md5Digest,
}

fn upload_text_field(name: &str) -> Option<UploadTextField> {
    match name {
        ":action" => Some(UploadTextField::Action),
        "metadata_version" => Some(UploadTextField::MetadataVersion),
        "name" => Some(UploadTextField::Name),
        "version" => Some(UploadTextField::Version),
        "requires_python" => Some(UploadTextField::RequiresPython),
        "license" => Some(UploadTextField::License),
        "license_expression" => Some(UploadTextField::LicenseExpression),
        "license_file" | "license_files" => Some(UploadTextField::LicenseFile),
        "provides_extra" | "provides_extras" => Some(UploadTextField::ProvidesExtra),
        "project_urls" => Some(UploadTextField::ProjectUrl),
        "home_page" => Some(UploadTextField::HomePage),
        "filetype" => Some(UploadTextField::Filetype),
        "sha256_digest" => Some(UploadTextField::Sha256Digest),
        "blake2_256_digest" => Some(UploadTextField::Blake2Digest),
        "md5_digest" => Some(UploadTextField::Md5Digest),
        _ => None,
    }
}

fn set_upload_text_field(form: &mut UploadForm, field: UploadTextField, value: String) {
    match field {
        UploadTextField::Action => form.action = Some(value),
        UploadTextField::MetadataVersion => form.metadata_version = Some(value),
        UploadTextField::Name => form.name = Some(value),
        UploadTextField::Version => form.version = Some(value),
        UploadTextField::RequiresPython => form.requires_python = Some(value),
        UploadTextField::License => form.license = Some(value),
        UploadTextField::LicenseExpression => form.license_expression = Some(value),
        UploadTextField::LicenseFile => form.license_files.push(value),
        UploadTextField::ProvidesExtra => form.provides_extra.push(value),
        UploadTextField::ProjectUrl => form.project_urls.push(value),
        UploadTextField::HomePage => form.home_page = Some(value),
        UploadTextField::Filetype => form.filetype = Some(value),
        UploadTextField::Sha256Digest => form.sha256_digest = Some(value),
        UploadTextField::Blake2Digest => form.blake2_256_digest = Some(value),
        UploadTextField::Md5Digest => form.md5_digest = Some(value),
    }
}

async fn read_text_field(mut field: axum::extract::multipart::Field<'_>, name: &str) -> Result<String, Response> {
    let mut bytes = Vec::new();
    while let Some(chunk) = field.chunk().await.map_err(reject)? {
        if bytes.len().saturating_add(chunk.len()) > MAX_UPLOAD_TEXT_FIELD_BYTES {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("upload field {name:?} exceeds {MAX_UPLOAD_TEXT_FIELD_BYTES} bytes"),
            )
                .into_response());
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(reject)
}

async fn drain_field(mut field: axum::extract::multipart::Field<'_>) -> Result<(), Response> {
    while field.chunk().await.map_err(reject)?.is_some() {}
    Ok(())
}

async fn stage_content(
    mut field: axum::extract::multipart::Field<'_>,
    blobs: &velodex_storage::blob::BlobStore,
) -> Result<StagedUpload, Response> {
    let mut pending = blobs.begin().map_err(storage_reject)?;
    let mut blake2 = Blake2bVar::new(32).expect("blake2b-256 output size is valid");
    while let Some(chunk) = field.chunk().await.map_err(reject)? {
        blake2.update(&chunk);
        pending.write(&chunk).map_err(storage_reject)?;
    }
    let mut digest = [0; 32];
    blake2
        .finalize_variable(&mut digest)
        .expect("blake2b-256 output buffer has the requested size");
    Ok(StagedUpload {
        blob: pending.finish().map_err(storage_reject)?,
        blake2_256: hex(&digest),
    })
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Map any multipart read or decode failure to a 400 response.
fn reject(err: impl std::fmt::Display) -> Response {
    (StatusCode::BAD_REQUEST, format!("bad upload: {err}")).into_response()
}

fn storage_reject(err: impl std::fmt::Display) -> Response {
    tracing::error!(error = %err, "upload staging failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("upload staging: blob store error: {err}"),
    )
        .into_response()
}

fn upload_error_response(err: &UploadError) -> Response {
    upload_error_message(err).into_response()
}

fn upload_error_message(err: &UploadError) -> (StatusCode, String) {
    match err {
        UploadError::NotFileUpload => (StatusCode::BAD_REQUEST, "unsupported :action".to_owned()),
        UploadError::Missing(field) => (StatusCode::BAD_REQUEST, format!("missing required field: {field}")),
        UploadError::InvalidName(name) => (
            StatusCode::BAD_REQUEST,
            format!(
                "invalid project name {name:?}: names must start and end with an ASCII letter or digit and contain only letters, digits, '.', '_' or '-'"
            ),
        ),
        UploadError::InvalidVersion(version) => (
            StatusCode::BAD_REQUEST,
            format!("invalid version {version:?}: expected a PEP 440 version"),
        ),
        UploadError::InvalidFilename(filename) => (
            StatusCode::BAD_REQUEST,
            format!(
                "invalid filename {filename:?}: filenames must be relative path segments without separators, traversal, or control characters"
            ),
        ),
        UploadError::InvalidDistributionFilename { filename, error } => (
            StatusCode::BAD_REQUEST,
            format!(
                "invalid distribution filename {filename:?}: {}",
                distribution_filename_error_message(error)
            ),
        ),
        UploadError::FiletypeMismatch { expected, actual } => (
            StatusCode::BAD_REQUEST,
            format!("filetype {actual:?} does not match filename; expected {expected:?}"),
        ),
        UploadError::FilenameNameMismatch { filename, form } => (
            StatusCode::BAD_REQUEST,
            format!("filename project {filename:?} does not match upload name {form:?}"),
        ),
        UploadError::FilenameVersionMismatch { filename, form } => (
            StatusCode::BAD_REQUEST,
            format!("filename version {filename:?} does not match upload version {form:?}"),
        ),
        UploadError::DigestMismatch(field) => (StatusCode::BAD_REQUEST, format!("{field} mismatch")),
        UploadError::Md5Only => (
            StatusCode::BAD_REQUEST,
            "md5_digest is not accepted without a sha256_digest or blake2_256_digest".to_owned(),
        ),
        UploadError::InvalidDigest { field, value } => (
            StatusCode::BAD_REQUEST,
            format!("{field} value {value:?} is not lowercase hex with the expected length"),
        ),
        UploadError::InvalidRequiresPython(value) => (
            StatusCode::BAD_REQUEST,
            format!("invalid Requires-Python value {value:?}: expected PEP 440 version specifiers"),
        ),
        UploadError::InvalidContent(message) => (
            StatusCode::BAD_REQUEST,
            format!("uploaded content does not match the filename format: {message}"),
        ),
        UploadError::InvalidMetadataUtf8 => (
            StatusCode::BAD_REQUEST,
            "artifact metadata is not valid UTF-8".to_owned(),
        ),
        UploadError::MetadataNameMismatch { metadata, form } => (
            StatusCode::BAD_REQUEST,
            format!("metadata Name {metadata:?} does not match upload name {form:?}"),
        ),
        UploadError::MetadataVersionMismatch { metadata, form } => (
            StatusCode::BAD_REQUEST,
            format!("metadata Version {metadata:?} does not match upload version {form:?}"),
        ),
        UploadError::MetadataFieldMismatch { field, metadata, form } => {
            upload_metadata_field_mismatch_message(field, metadata, form)
        }
        UploadError::InvalidUploadTime => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "configured clock produced an invalid upload timestamp".to_owned(),
        ),
    }
}

fn upload_metadata_field_mismatch_message(field: &str, metadata: &str, form: &str) -> (StatusCode, String) {
    (
        StatusCode::BAD_REQUEST,
        format!("metadata {field} {metadata:?} does not match upload value {form:?}"),
    )
}

fn distribution_filename_error_message(err: &DistributionFilenameError) -> String {
    match err {
        DistributionFilenameError::UnsupportedExtension => "accepted upload formats are .whl and .tar.gz".to_owned(),
        DistributionFilenameError::LegacyEgg => {
            "legacy .egg uploads are not accepted; upload a wheel or .tar.gz sdist".to_owned()
        }
        DistributionFilenameError::InvalidWheelShape => {
            "wheel filenames must use distribution-version(-build tag)?-python tag-abi tag-platform tag.whl".to_owned()
        }
        DistributionFilenameError::InvalidSdistShape => "sdist filenames must use name-version.tar.gz".to_owned(),
        DistributionFilenameError::InvalidName(name) => {
            format!("distribution name component {name:?} is not a valid PyPA project name")
        }
        DistributionFilenameError::InvalidVersion(version) => {
            format!("version component {version:?} is not a PEP 440 version")
        }
        DistributionFilenameError::InvalidTag(tag) => {
            format!("wheel build/tag component {tag:?} contains invalid characters")
        }
    }
}

/// `GET /api-docs/openapi.json` — the `OpenAPI` description of this server.
pub async fn openapi_spec() -> Response {
    static SPEC: std::sync::LazyLock<String> = std::sync::LazyLock::new(crate::api::openapi_json);
    ([(header::CONTENT_TYPE, "application/json")], SPEC.as_str()).into_response()
}

/// `GET /+api` — API discovery and copyable client configuration for every configured index.
pub async fn api(State(state): State<Arc<AppState>>, OriginalUri(uri): OriginalUri, headers: HeaderMap) -> Response {
    let base = BaseUrl::from_request(&headers, &uri);
    axum::Json(discovery::root_document(&state, base.as_ref())).into_response()
}

/// `GET /+search` — search cached packages across configured indexes.
pub async fn search(State(state): State<Arc<AppState>>, OriginalUri(uri): OriginalUri) -> Response {
    match SearchParams::from_query(uri.query()) {
        Ok(params) => search_response(&state, params),
        Err(err) => search_error_response(&err),
    }
}

fn index_api(state: &AppState, position: usize, base: Option<&BaseUrl>) -> Response {
    axum::Json(discovery::index_document(
        describe_index(&state.indexes, position),
        base,
    ))
    .into_response()
}

/// The `/+status` detail selector.
#[derive(Debug, serde::Deserialize)]
pub struct StatusQuery {
    details: Option<String>,
}

/// `GET /+status` — health, identity, counters, and the configured indexes. The web UI's live
/// dashboard refreshes from this document.
pub async fn status(State(state): State<Arc<AppState>>, Query(query): Query<StatusQuery>) -> Response {
    let serial = state.meta.current_serial().unwrap_or(0);
    let summaries = (query.details.as_deref() == Some("admin")).then(|| {
        let index_names = state.indexes.iter().map(|index| index.name.clone()).collect::<Vec<_>>();
        state
            .meta
            .summarize_indexes(&index_names, STATUS_RECENT_UPLOADS)
            .unwrap_or_default()
    });
    let indexes: Vec<serde_json::Value> = state
        .describe_indexes()
        .into_iter()
        .map(|index| {
            let mut object = serde_json::Map::from_iter([
                ("name".to_owned(), serde_json::json!(index.name)),
                ("route".to_owned(), serde_json::json!(index.route)),
                ("kind".to_owned(), serde_json::json!(index.kind)),
                ("layers".to_owned(), serde_json::json!(index.layers)),
                ("uploads".to_owned(), serde_json::json!(index.uploads)),
                ("volatile_deletes".to_owned(), serde_json::json!(index.volatile_deletes)),
                ("upload_to".to_owned(), serde_json::json!(index.upload_to)),
                (
                    "upstream".to_owned(),
                    serde_json::json!(index.upstream.map(|upstream| serde_json::json!({
                        "url": upstream.url,
                        "auth": {
                            "kind": upstream.auth,
                            "redacted": (upstream.auth != "none").then_some("<redacted>"),
                        },
                        "offline": upstream.offline,
                        "status": "configured",
                    }))),
                ),
                (
                    "local".to_owned(),
                    serde_json::json!(index.local.map(|local| serde_json::json!({
                        "volatile": local.volatile,
                        "upload_token": {
                            "configured": local.upload_token.configured,
                            "redacted": local.upload_token.redacted,
                        },
                    }))),
                ),
            ]);
            if let Some(summaries) = &summaries {
                let summary = summaries.get(&index.name).cloned().unwrap_or_default();
                object.insert("project_count".to_owned(), serde_json::json!(summary.project_count));
                object.insert("upload_count".to_owned(), serde_json::json!(summary.upload_count));
                object.insert(
                    "recent_uploads".to_owned(),
                    serde_json::json!(
                        summary
                            .recent_uploads
                            .into_iter()
                            .map(|upload| {
                                serde_json::json!({
                                    "project": upload.project,
                                    "filename": upload.filename,
                                    "version": upload.version,
                                    "uploaded_at": upload.uploaded_at,
                                    "size": upload.size,
                                })
                            })
                            .collect::<Vec<_>>()
                    ),
                );
            }
            serde_json::Value::Object(object)
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
    write_rate_limit_metrics(&mut body, &state);
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

fn write_rate_limit_metrics(body: &mut String, state: &AppState) {
    let _ = writeln!(
        body,
        "# HELP velodex_rate_limit_allowed_total HTTP requests allowed by the local rate limiter.\n\
         # TYPE velodex_rate_limit_allowed_total counter"
    );
    for counter in state.rate_limits.counters() {
        let _ = writeln!(
            body,
            "velodex_rate_limit_allowed_total{{class=\"{}\"}} {}",
            counter.class, counter.allowed
        );
    }
    let _ = writeln!(
        body,
        "# HELP velodex_rate_limit_denied_total HTTP requests denied by the local rate limiter.\n\
         # TYPE velodex_rate_limit_denied_total counter"
    );
    for counter in state.rate_limits.counters() {
        let _ = writeln!(
            body,
            "velodex_rate_limit_denied_total{{class=\"{}\"}} {}",
            counter.class, counter.denied
        );
    }
    let _ = writeln!(
        body,
        "# HELP velodex_upstream_rate_limit_denied_total Upstream fetches denied by the local concurrency cap.\n\
         # TYPE velodex_upstream_rate_limit_denied_total counter"
    );
    for counter in state.upstream_limits.snapshots() {
        let _ = writeln!(
            body,
            "velodex_upstream_rate_limit_denied_total{{index=\"{}\"}} {}",
            counter.index, counter.denied
        );
    }
    let _ = writeln!(
        body,
        "# HELP velodex_upstream_inflight_fetches Current upstream fetches held by the local concurrency cap.\n\
         # TYPE velodex_upstream_inflight_fetches gauge"
    );
    for counter in state.upstream_limits.snapshots() {
        let _ = writeln!(
            body,
            "velodex_upstream_inflight_fetches{{index=\"{}\"}} {}",
            counter.index, counter.in_flight
        );
    }
}

#[cfg(test)]
mod tests {
    use velodex_storage::blob::BlobError;
    use velodex_storage::meta::MetaError;

    use super::*;

    #[test]
    fn test_cache_error_status_maps_store_and_policy_errors() {
        let context = CacheContext::mutation("file removal");
        assert_eq!(
            cache_error_status(&CacheError::Meta(meta_error()), &context),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            cache_error_status(
                &CacheError::Blob(BlobError::NotFound("sha256:abc".to_owned())),
                &context
            ),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            cache_error_status(&CacheError::FileExists("pkg-1.0.whl".to_owned()), &context),
            StatusCode::CONFLICT
        );
        assert_eq!(
            cache_error_status(&CacheError::NotVolatile, &context),
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn test_upload_status_response_maps_policy_and_store_errors() {
        assert!(upload_status_response(Ok(ProjectStatus::Active), "root/pypi", "flask").is_none());
        let archived = upload_status_response(Ok(ProjectStatus::Archived), "root/pypi", "flask").unwrap();
        assert_eq!(archived.response.status(), StatusCode::FORBIDDEN);
        assert_eq!(archived.result, "denied");
        assert_eq!(archived.reason, "project \"flask\" is archived; uploads are disabled");

        let failure = upload_status_response(Err(CacheError::Meta(meta_error())), "root/pypi", "flask").unwrap();
        assert_eq!(failure.response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(failure.result, "failure");
        assert!(failure.reason.contains("metadata store error"));
    }

    fn meta_error() -> MetaError {
        MetaError::Decode(serde_json::from_str::<serde_json::Value>("not json").unwrap_err())
    }
}
