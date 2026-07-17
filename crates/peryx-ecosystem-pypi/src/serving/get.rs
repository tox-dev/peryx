//! `PyPI` GET routing: project list, project detail, release files, and archive inspection dispatch.
#![allow(
    clippy::result_large_err,
    reason = "handler helpers carry an axum Response as their error; boxing it everywhere adds noise"
)]

use std::sync::Arc;
use std::time::SystemTime;

use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use peryx_core::path::{self};
use peryx_driver::conditional::{applicable_range, http_date, if_modified_since, if_none_match, last_modified};
use peryx_driver::not_found;
use peryx_driver::range::{RangeSpec, parse_range, unsatisfiable_range};
use peryx_driver::state::ServingState;
use peryx_events::metrics::Event;
use peryx_index::Index;
use peryx_policy::PolicyAction;
use peryx_storage::blob::Digest;

use crate::cache::{self, CacheError, PageOutcome};
use crate::normalize_name;
use crate::policy::PypiPolicy;

use super::inspect::inspect_route;
use super::response::{
    CacheContext, cache_error_response, detail_response, file_response, html_bytes_response, index_response,
    json_bytes_response, legacy_bytes_response, legacy_json_response, policy_denial_response,
};
use super::{Format, METADATA_FAMILY, negotiate, path_error_response, safe_filename};

/// `GET /{route}/...` serves the project list, project detail, or a file/metadata download for the
/// index the neutral router already resolved to `position`. The peryx-owned `+api`/`+search` routes run
/// before this, and the router routes only this ecosystem's indexes here, so only its paths arrive.
pub async fn pypi_dispatch_get(
    state: Arc<ServingState>,
    position: usize,
    rest: &str,
    uri: axum::http::Uri,
    headers: HeaderMap,
    head: bool,
) -> Response {
    pypi_get(&state, position, rest, &headers, &uri, head).await
}

/// `PyPI` GET routing within an index: the Simple index and project detail (HTML, PEP 691 JSON, legacy
/// JSON), release files, and archive inspection.
///
/// Only the file route reads `head`. Everywhere else the answer is a page peryx has to produce anyway
/// to know its status, and axum drops the body of it; a file is the one representation whose body costs
/// an upstream download.
async fn pypi_get(
    state: &Arc<ServingState>,
    position: usize,
    rest: &str,
    headers: &HeaderMap,
    uri: &axum::http::Uri,
    head: bool,
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
            if let Some((bytes, last_serial)) =
                state.hot_fresh_versioned(&state.hot_key(&index.route, &target.project, &variant))
            {
                return legacy_bytes_response(bytes, last_serial);
            }
            let detail = cache::resolve_detail_page(state, index, &target.project, &index.route).await;
            if let Ok(Some(found)) = &detail
                && let Some(body) = crate::legacy_json::render_legacy_json_with_serial(
                    &found.detail,
                    target.version.as_deref(),
                    None,
                    found.last_serial,
                )
            {
                let body = bytes::Bytes::from(body);
                remember_rendered(state, index, &target.project, &variant, &body, found.last_serial);
                return legacy_bytes_response(body, found.last_serial);
            }
            return legacy_json_response(detail, &index.route, &target.project, target.version.as_deref());
        }
        Ok(None) => {}
        Err(response) => return response,
    }
    if rest == "simple" {
        return simple_slash_redirect(uri, rest, "simple/");
    }
    if rest == "simple/" {
        return simple_index_response(state, index, headers);
    }
    if let Some(project) = rest
        .strip_prefix("simple/")
        .filter(|rest| !rest.is_empty() && !rest.contains('/'))
    {
        return simple_slash_redirect(uri, rest, &format!("simple/{}/", normalize_name(project)));
    }
    if let Some(project) = rest.strip_prefix("simple/").and_then(|rest| rest.strip_suffix('/')) {
        let normalized = normalize_name(project);
        state.metrics.record(Event::Page {
            route: index.route.clone(),
            project: normalized.clone(),
        });
        if matches!(negotiate(headers), Format::Json) {
            match cache::stream_detail(state.clone(), position, normalized.clone()).await {
                Ok(PageOutcome::Ready(bytes, last_serial)) => {
                    return json_bytes_response(bytes, last_serial);
                }
                Ok(PageOutcome::Streaming(stream, last_serial)) => {
                    return json_bytes_response(axum::body::Body::from_stream(stream), last_serial);
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
            if let Some((bytes, last_serial)) =
                state.hot_fresh_versioned(&state.hot_key(&index.route, &normalized, cache::SIMPLE_HTML))
            {
                return html_bytes_response(bytes, last_serial);
            }
            let detail = cache::resolve_detail_page(state, index, &normalized, &index.route).await;
            if let Ok(Some(found)) = &detail {
                let body = bytes::Bytes::from(crate::render_detail_html(&found.detail));
                remember_rendered(state, index, &normalized, cache::SIMPLE_HTML, &body, found.last_serial);
                return html_bytes_response(body, found.last_serial);
            }
            return detail_response(detail, &index.route, &normalized);
        }
        let detail = cache::resolve_detail_page(state, index, &normalized, &index.route).await;
        return detail_response(detail, &index.route, &normalized);
    }
    if let Some(file) = rest.strip_prefix("files/") {
        return file_route(state, index, file, headers, head).await;
    }
    if let Some(target) = rest.strip_prefix("inspect/") {
        return inspect_route(state.clone(), index.route.clone(), target, uri.query()).await;
    }
    not_found()
}

fn simple_index_response(state: &ServingState, index: &Index, headers: &HeaderMap) -> Response {
    let list = cache::resolve_list(state, index)
        .and_then(|list| cache::list_serial(state, index).map(|last_serial| (list, last_serial)));
    index_response(list, negotiate(headers), &index.route)
}

/// PEP 503 canonical Simple URLs end in a slash; a request that drops it is redirected rather than
/// 404'd, matching Warehouse's `301`. `rest` is a suffix of the request path, so stripping it leaves
/// the index's route prefix to prepend to the canonical tail. The query string is carried across.
fn simple_slash_redirect(uri: &axum::http::Uri, rest: &str, canonical_tail: &str) -> Response {
    let path = uri.path();
    let mut location = format!("{}{canonical_tail}", &path[..path.len() - rest.len()]);
    if let Some(query) = uri.query() {
        location.push('?');
        location.push_str(query);
    }
    (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, location)], "").into_response()
}

async fn file_route(state: &Arc<ServingState>, index: &Index, file: &str, headers: &HeaderMap, head: bool) -> Response {
    let route = index.route.clone();
    let Some((sha256, raw_filename)) = file.split_once('/') else {
        return not_found();
    };
    let digest = match super::parse_digest(sha256) {
        Ok(digest) => digest,
        Err(err) => return path_error_response(&err),
    };
    let filename = match safe_filename(raw_filename) {
        Ok(filename) => filename,
        Err(err) => return path_error_response(&err),
    };
    match download_policy_response(state, index, &filename, &digest).await {
        Ok(Some(response)) => return response,
        Ok(None) => {}
        Err(err) => return cache_error_response(&err, CacheContext::file(&route, digest.as_str(), &filename)),
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
    let etag = format!("\"{}\"", digest.as_str());
    if let Some(response) = not_modified(headers, &etag) {
        return response;
    }
    let range = applicable_range(headers, &etag);
    if head {
        return head_blob(
            state,
            &route,
            &filename,
            &digest,
            range,
            &etag,
            conditional_date(headers),
        )
        .await;
    }
    serve_blob(state, route, &filename, digest, range, &etag, conditional_date(headers)).await
}

const IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// The `304` a client holding these bytes earns, or nothing when it holds other bytes.
///
/// RFC 9110 s13.1.2 puts this condition ahead of the method and of `Range`, and the access and
/// download-policy checks have run by now, so a match answers the request before anything opens the
/// blob or fetches it from upstream.
///
/// A digest this index has never cached matches all the same: the URL names the bytes, so a client
/// holding them holds the current representation whether or not the store does.
fn not_modified(headers: &HeaderMap, etag: &str) -> Option<Response> {
    let field = headers.get(header::IF_NONE_MATCH)?.to_str().ok()?;
    if_none_match(field, etag).then(|| unchanged(etag, None))
}

/// The `If-Modified-Since` date this request leaves any say in, if it sent one.
///
/// RFC 9110 s13.1.3: an `If-None-Match` supersedes it, matched or not. A client that sent both asked
/// to be judged on the exact validator, and answering the date after the tag has already refused would
/// serve a `304` for bytes the client just said it does not hold.
fn conditional_date(headers: &HeaderMap) -> Option<&str> {
    if headers.contains_key(header::IF_NONE_MATCH) {
        return None;
    }
    headers.get(header::IF_MODIFIED_SINCE)?.to_str().ok()
}

/// The bodyless `304`: the metadata a `200` would have carried, minus the body.
///
/// The entity tag is answered from the request line, off a digest the store need never have cached, so
/// the date rides along only where one was read: the blob the condition was evaluated against.
fn unchanged(etag: &str, modified: Option<SystemTime>) -> Response {
    let mut builder = Response::builder()
        .status(StatusCode::NOT_MODIFIED)
        .header(header::ETAG, etag)
        .header(header::CACHE_CONTROL, IMMUTABLE)
        .header(header::ACCEPT_RANGES, "bytes");
    if let Some(modified) = modified {
        builder = builder.header(header::LAST_MODIFIED, http_date(modified));
    }
    builder
        .body(axum::body::Body::empty())
        .expect("not-modified response builds from validated header parts")
}

async fn download_policy_response(
    state: &ServingState,
    index: &Index,
    filename: &str,
    digest: &Digest,
) -> Result<Option<Response>, cache::CacheError> {
    // No configured policy can deny a download, so skip the two blocking stats it would take to
    // learn the file size. This is the zero-config default and keeps the warm wheel path off the
    // filesystem until the byte stream itself opens the file.
    if !index.policy.active() {
        return Ok(None);
    }
    let size = if let Some(metadata) = state.blobs.head(digest).await? {
        Some(metadata.bytes)
    } else {
        cache::registered_file_size(state, digest)?
    };
    Ok(index
        .policy
        .check_download(PolicyAction::Serve, filename, size)
        .err()
        .map(|denial| policy_denial_response(&denial)))
}

struct LegacyJsonTarget {
    project: String,
    version: Option<String>,
}

fn legacy_json_target(rest: &str) -> Result<Option<LegacyJsonTarget>, Response> {
    // The Simple API and the file/inspect routes own their namespaces; a project normalized to `json`
    // must reach `GET .../simple/json/`, not be claimed here as the legacy JSON view of `simple`.
    if ["simple/", "files/", "inspect/"]
        .iter()
        .any(|prefix| rest.starts_with(prefix))
    {
        return Ok(None);
    }
    let trimmed = rest.trim_end_matches('/');
    let Some(spec) = trimmed.strip_suffix("/json") else {
        return Ok(None);
    };
    let Some((project, version)) = spec.split_once('/') else {
        let project = path::decode_path_segment(spec).map_err(|err| path_error_response(&err))?;
        path::validate_path_segment("project", &project).map_err(|err| path_error_response(&err))?;
        return Ok(Some(LegacyJsonTarget {
            project: normalize_name(&project),
            version: None,
        }));
    };
    let project = path::decode_path_segment(project).map_err(|err| path_error_response(&err))?;
    let version = path::decode_path(version).map_err(|err| path_error_response(&err))?;
    path::validate_path_segment("project", &project).map_err(|err| path_error_response(&err))?;
    path::validate_path_segment("version", &version).map_err(|err| path_error_response(&err))?;
    Ok(Some(LegacyJsonTarget {
        project: normalize_name(&project),
        version: Some(version.into_owned()),
    }))
}

/// What every representation of an artifact carries, whatever the method and whatever the store holds.
const fn blob_headers(etag: &str) -> [(header::HeaderName, &str); 4] {
    [
        (header::CONTENT_TYPE, "application/octet-stream"),
        (header::CACHE_CONTROL, IMMUTABLE),
        (header::ACCEPT_RANGES, "bytes"),
        (header::ETAG, etag),
    ]
}

/// Answer a file `HEAD` with the headers of the `GET` it stands for and no body.
///
/// Nothing here opens the artifact or asks upstream for it, which is the point: a probe of an uncached
/// wheel used to start the whole download — hashed, written, and paid for in bandwidth — for a client
/// that cannot receive a byte of it.
///
/// A cached blob answers a `Range` the way the matching `GET` does. An uncached one has no seekable
/// body behind it, so its `GET` streams the whole representation and ignores the `Range`; the `HEAD`
/// says the same. Its `Content-Length` is the size the index page registered, and is omitted when the
/// page carried none: an uncached artifact's length is not peryx's to invent.
async fn head_blob(
    state: &Arc<ServingState>,
    route: &str,
    filename: &str,
    digest: &Digest,
    range: Option<&str>,
    etag: &str,
    since: Option<&str>,
) -> Response {
    let probe = match cache::probe_file(state, digest).await {
        Ok(probe) => probe,
        Err(err) => return cache_error_response(&err, CacheContext::file(route, digest.as_str(), filename)),
    };
    let (status, length, content_range, modified) = match probe {
        cache::FileProbe::Cached(size, stored) => {
            let modified = stored.map(|stored| last_modified(stored, SystemTime::now()));
            // The condition outranks the range, as it does for the GET this describes.
            if let Some(modified) = modified
                && since.is_some_and(|field| if_modified_since(field, modified))
            {
                return unchanged(etag, Some(modified));
            }
            match range.map_or(RangeSpec::Ignore, |value| parse_range(value, size)) {
                RangeSpec::Ignore => (StatusCode::OK, Some(size), None, modified),
                RangeSpec::Unsatisfiable => return unsatisfiable_range(size),
                RangeSpec::Satisfiable(start, end) => (
                    StatusCode::PARTIAL_CONTENT,
                    Some(end - start + 1),
                    Some(format!("bytes {start}-{end}/{size}")),
                    modified,
                ),
            }
        }
        // An uncached blob has no write to date, the way the teed GET has none to state.
        cache::FileProbe::Upstream(size) => (StatusCode::OK, size, None, None),
    };
    let mut builder = Response::builder().status(status);
    for (name, value) in blob_headers(etag) {
        builder = builder.header(name, value);
    }
    if let Some(modified) = modified {
        builder = builder.header(header::LAST_MODIFIED, http_date(modified));
    }
    if let Some(length) = length {
        builder = builder.header(header::CONTENT_LENGTH, length);
    }
    if let Some(content_range) = content_range {
        builder = builder.header(header::CONTENT_RANGE, content_range);
    }
    // An empty body has an exact size, so hyper would infer `Content-Length: 0` and tell the client
    // the artifact holds nothing. A stream has no size to infer, which leaves the length the header
    // above states, or none when the index page published none, the way the teed GET answers.
    let body = length.map_or_else(
        || axum::body::Body::from_stream(futures_util::stream::empty::<Result<bytes::Bytes, std::io::Error>>()),
        |_| axum::body::Body::empty(),
    );
    builder
        .body(body)
        .expect("head response builds from validated header parts")
}

/// Stream a blob to the client: from disk when cached, teed from the upstream cache otherwise.
///
/// A cached blob honors a single-range request, which is how pip resumes an interrupted wheel
/// download. A blob still being teed from upstream has no seekable body to slice, so a range over it
/// falls back to the whole `200` representation the client asked to resume.
///
/// The cached blob also carries the date the store wrote it, which is the one modification date peryx
/// can stand behind: the digest fixes the bytes, so the only thing that can change under this URL is
/// which side of the cache serves them. A blob still arriving from upstream has no such date — the
/// write it would name has not happened — so it goes out with the tag alone, as it did before.
async fn serve_blob(
    state: &Arc<ServingState>,
    route: String,
    filename: &str,
    digest: Digest,
    range: Option<&str>,
    etag: &str,
    since: Option<&str>,
) -> Response {
    let digest_hex = digest.as_str().to_owned();
    let blob_headers = blob_headers(etag);
    match cache::stream_file(state.clone(), digest.clone(), route.clone(), filename.to_owned()).await {
        Ok(cache::FileOutcome::Cached(metadata)) => {
            let size = metadata.bytes;
            let modified = metadata.modified.map(|stored| last_modified(stored, SystemTime::now()));
            // RFC 9110 s13.2.2 evaluates the condition ahead of the range: a client whose copy is still
            // current gets the `304` it asked for, not the slice of it that a `Range` would have cut.
            if let Some(modified) = modified
                && since.is_some_and(|field| if_modified_since(field, modified))
            {
                return unchanged(etag, Some(modified));
            }
            let (status, start, length, content_range) =
                match range.map_or(RangeSpec::Ignore, |value| parse_range(value, size)) {
                    RangeSpec::Ignore => (StatusCode::OK, 0, size, None),
                    RangeSpec::Unsatisfiable => return unsatisfiable_range(size),
                    RangeSpec::Satisfiable(start, end) => (
                        StatusCode::PARTIAL_CONTENT,
                        start,
                        end - start + 1,
                        Some(format!("bytes {start}-{end}/{size}")),
                    ),
                };
            let mut builder = Response::builder()
                .status(status)
                .header(header::CONTENT_LENGTH, length);
            for (name, value) in blob_headers {
                builder = builder.header(name, value);
            }
            if let Some(modified) = modified {
                builder = builder.header(header::LAST_MODIFIED, http_date(modified));
            }
            if let Some(content_range) = content_range {
                builder = builder.header(header::CONTENT_RANGE, content_range);
            }
            let read = match state.blobs.open(&digest, Some(start..start + length)).await {
                Ok(read) => read,
                Err(err) => {
                    tracing::error!(error = ?err, digest = digest_hex, "cached blob open failed");
                    return (
                        StatusCode::NOT_FOUND,
                        format!("cached file missing on index {route:?}: digest {digest_hex}, filename {filename:?}"),
                    )
                        .into_response();
                }
            };
            let metrics = state.metrics.clone();
            let project = crate::project_of_filename(filename);
            let filename = filename.to_owned();
            let body = peryx_driver::body::on_body_complete(peryx_driver::body::blob_read(read), move |bytes| {
                metrics.record(Event::Download {
                    project,
                    route,
                    filename,
                    bytes,
                });
            });
            builder
                .body(body)
                .expect("blob response builds from validated header parts")
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
/// Keep a rendered page under the project epoch that is current *now*, not the one the request started
/// with.
///
/// Resolving a cold page fetches it from upstream and persists it, and persisting bumps that project's
/// epoch. A key captured before that carries the old epoch, so the entry it writes is one no later
/// reader can compute: the cache would fill and never hit.
fn remember_rendered(
    state: &ServingState,
    index: &Index,
    project: &str,
    variant: &str,
    body: &bytes::Bytes,
    last_serial: Option<u64>,
) {
    if let Ok(Some(expires_at)) = cache::rendered_expiry(state, index, project) {
        let key = state.hot_key(&index.route, project, variant);
        state
            .cache
            .store_hot_versioned(key, body.clone(), expires_at, last_serial);
    }
}
