//! `PyPI` GET routing: project list, project detail, release files, and archive inspection dispatch.
#![allow(
    clippy::result_large_err,
    reason = "handler helpers carry an axum Response as their error; boxing it everywhere adds noise"
)]

use std::sync::Arc;

use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use peryx_events::metrics::Event;
use peryx_http::handlers::not_found;
use peryx_http::path_safety::{self};
use peryx_http::state::AppState;
use peryx_index::Index;
use peryx_policy::PolicyAction;
use peryx_storage::blob::Digest;

use crate::cache::{self, CacheError, PageOutcome};
use crate::normalize_name;
use crate::policy::PypiPolicy;

use super::inspect::inspect_route;
use super::response::{
    CacheContext, cache_error_response, detail_response, file_response, html_bytes_response, index_response,
    legacy_bytes_response, legacy_json_response, policy_denial_response,
};
use super::{Format, METADATA_FAMILY, MIME_JSON, negotiate, path_error_response, safe_filename};

/// `GET /{route}/...` serves the project list, project detail, or a file/metadata download for the
/// index the neutral router already resolved to `position`. The peryx-owned `+api`/`+search` routes run
/// before this, and the router routes only this ecosystem's indexes here, so only its paths arrive.
pub async fn pypi_dispatch_get(
    state: Arc<AppState>,
    position: usize,
    rest: &str,
    uri: axum::http::Uri,
    headers: HeaderMap,
) -> Response {
    pypi_get(&state, position, rest, &headers, &uri).await
}

/// `PyPI` GET routing within an index: the Simple index and project detail (HTML, PEP 691 JSON, legacy
/// JSON), release files, and archive inspection.
async fn pypi_get(
    state: &Arc<AppState>,
    position: usize,
    rest: &str,
    headers: &HeaderMap,
    uri: &axum::http::Uri,
) -> Response {
    let index = state.index_at(position);
    match legacy_json_target(rest) {
        Ok(Some(target)) => {
            state.metrics.record(Event::Page {
                route: index.route.clone(),
                project: target.project.clone(),
            });
            // The release form of this API answers differently per version, so the version is part of
            // what the cached bytes are.
            let variant = target.version.as_deref().map_or_else(
                || cache::LEGACY_JSON.to_owned(),
                |version| format!("{}/{version}", cache::LEGACY_JSON),
            );
            if let Some(bytes) = state.hot_fresh(&state.hot_key(&index.route, &target.project, &variant)) {
                return legacy_bytes_response(bytes);
            }
            let detail = cache::resolve_detail(state, index, &target.project, &index.route).await;
            if let Ok(Some(found)) = &detail
                && let Some(body) = crate::render_legacy_json(found, target.version.as_deref())
            {
                let body = bytes::Bytes::from(body);
                remember_rendered(state, index, &target.project, &variant, &body);
                return legacy_bytes_response(body);
            }
            return legacy_json_response(detail, &index.route, &target.project, target.version.as_deref());
        }
        Ok(None) => {}
        Err(response) => return response,
    }
    if rest == "simple/" {
        return index_response(cache::resolve_list(state, index), negotiate(headers), &index.route);
    }
    if let Some(project) = rest.strip_prefix("simple/").and_then(|rest| rest.strip_suffix('/')) {
        let normalized = normalize_name(project);
        state.metrics.record(Event::Page {
            route: index.route.clone(),
            project: normalized.clone(),
        });
        if matches!(negotiate(headers), Format::Json) {
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
                    return detail_response(Err(err), &index.route, &normalized);
                }
                Err(err) => {
                    tracing::error!(error = ?err, "streaming page failed, serving buffered");
                }
            }
        }
        let index = state.index_at(position);
        let format = negotiate(headers);
        if matches!(format, Format::Html) {
            if let Some(bytes) = state.hot_fresh(&state.hot_key(&index.route, &normalized, cache::SIMPLE_HTML)) {
                return html_bytes_response(bytes);
            }
            let detail = cache::resolve_detail(state, index, &normalized, &index.route).await;
            if let Ok(Some(found)) = &detail {
                let body = bytes::Bytes::from(crate::render_detail_html(found));
                remember_rendered(state, index, &normalized, cache::SIMPLE_HTML, &body);
                return html_bytes_response(body);
            }
            return detail_response(detail, &index.route, &normalized);
        }
        let detail = cache::resolve_detail(state, index, &normalized, &index.route).await;
        return detail_response(detail, &index.route, &normalized);
    }
    if let Some(file) = rest.strip_prefix("files/") {
        return file_route(state, index, file).await;
    }
    if let Some(target) = rest.strip_prefix("inspect/") {
        return inspect_route(state.clone(), index.route.clone(), target, uri.query()).await;
    }
    not_found()
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
        state.metrics.record(Event::Ecosystem {
            route: route.clone(),
            project: crate::project_of_filename(&filename),
            filename: Some(filename.clone()),
            family: METADATA_FAMILY.key,
        });
        return file_response(
            cache::metadata_bytes(state, &digest, &route, &filename).await,
            CacheContext::metadata(&route, digest.as_str(), &filename),
        );
    }
    serve_blob(state, route, &filename, digest).await
}

fn download_policy_response(state: &AppState, index: &Index, filename: &str, digest: &Digest) -> Option<Response> {
    // No configured policy can deny a download, so skip the two blocking stats it would take to
    // learn the file size. This is the zero-config default and keeps the warm wheel path off the
    // filesystem until the byte stream itself opens the file.
    if !index.policy.active() {
        return None;
    }
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
        version: Some(version.into_owned()),
    }))
}

/// Stream a blob to the client: from disk when cached, teed from the upstream cache otherwise.
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
                project: crate::project_of_filename(filename),
                route,
                filename: filename.to_owned(),
                bytes,
            });
            // Pipeline the disk read ahead of the socket write: a pull-driven ReaderStream awaits each
            // read before writing it, serializing two independent I/O waits per chunk.
            let body = peryx_http::body::pipelined_file(file.into_std().await, 0, bytes);
            (blob_headers, body).into_response()
        }
        // A live stream records its download event at EOF, when the byte count exists.
        Ok(cache::FileOutcome::Live(stream)) => (blob_headers, axum::body::Body::from_stream(stream)).into_response(),
        Err(err) => {
            tracing::error!(error = ?err, "file stream failed");
            cache_error_response(&err, CacheContext::file(&route, &digest_hex, filename))
        }
    }
}

/// Keep a rendered representation for as long as the page it was rendered from stays fresh.
///
/// A miss costs the render again and nothing else, so a failure to cache is never a failure to serve.
/// Keep a rendered page under the epoch that is current *now*, not the one the request started with.
///
/// Resolving a cold page fetches it from upstream and persists it, and persisting bumps the epoch. A
/// key captured before that carries the old epoch, so the entry it writes is one no later reader can
/// compute: the cache would fill and never hit.
fn remember_rendered(state: &AppState, index: &Index, project: &str, variant: &str, body: &bytes::Bytes) {
    if let Ok(Some(expires_at)) = cache::rendered_expiry(state, index, project) {
        let key = state.hot_key(&index.route, project, variant);
        state.hot.insert(key, (expires_at, body.clone()));
    }
}
