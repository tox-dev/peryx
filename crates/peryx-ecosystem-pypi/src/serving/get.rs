use std::sync::Arc;
use std::time::SystemTime;

use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::FutureExt;
use peryx_core::path::{self};
use peryx_driver::access::ReadAccess;
use peryx_driver::conditional::{applicable_range, http_date, if_modified_since, if_none_match, last_modified};
use peryx_driver::not_found;
use peryx_driver::range::unsatisfiable_range;
use peryx_driver::state::ServingState;
use peryx_events::metrics::Observation;
use peryx_identity::{Denial, ResourceMatch};
use peryx_index::{Index, IndexKind};
use peryx_policy::{PolicyAction, PolicyDenial};
use peryx_storage::blob::{Digest, RangeRequest, parse_range};

use crate::cache::{self, CacheError, PageOutcome};
use crate::normalize_name;
use crate::policy::PypiPolicy;

use super::inspect::inspect_route;
use super::response::{
    CacheContext, DownloadRefusal, PageSerial, cache_error_response, detail_response, html_bytes_response,
    index_response, json_bytes_response, legacy_bytes_response, legacy_json_response, metadata_response, page_serial,
    provenance_response,
};
use super::{Format, HttpResult, METADATA_FAMILY, PROVENANCE_FAMILY, negotiate, path_error_response, safe_filename};
use crate::attestation;

/// On a replica, whether serving content at `last_serial` would expose metadata past the readable
/// frontier - a serial a required derived view (the search or blob view) has not caught up to yet. A
/// replica holds such a read and serves the older contiguous view (a surviving cached page) or a
/// not-found instead, so a search that misses the new metadata and a page that shows it never disagree,
/// and a listed file's blob is present before its page serves. The primary is never read-only, so its
/// freshness is unchanged.
///
/// A hosted index's `last_serial` is a local journal serial the frontier governs. A cached index
/// reports its upstream's serial, which it does not, so it is never gated. A virtual index carries no
/// serial of its own but may surface a hosted member's content, so it inherits its hosted members'
/// serials: the gate holds it until every hosted member it layers is readable. An unreadable frontier
/// reads as zero and a member whose serial cannot be read counts as past it, so a replica fails closed.
fn holds_below_readable_frontier(state: &ServingState, index: &Index, last_serial: Option<u64>) -> bool {
    if !state.read_only {
        return false;
    }
    let frontier = state.readable_frontier().unwrap_or_default().serial;
    match &index.kind {
        IndexKind::Hosted { .. } => last_serial.is_some_and(|serial| serial > frontier),
        IndexKind::Cached { .. } => false,
        // A virtual index is readable only up to the least-readable of its members, so it holds until
        // every hosted member it layers is readable: the max over its hosted members' serials past the
        // frontier. Hosted members share the local journal, so that max is the store's current serial,
        // which a member whose read fails counts as past the frontier, failing closed.
        IndexKind::Virtual { layers, .. } => {
            peryx_index::layers_include_hosted(&state.indexes, layers)
                && state.meta.current_serial().map_or(true, |serial| serial > frontier)
        }
    }
}

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
    let authenticated = headers.contains_key(header::AUTHORIZATION);
    let mut response = if let Some(denial) = read_denial(&state, position, rest, &headers) {
        denial
    } else {
        let mut response = pypi_get(&state, position, rest, &headers, &uri, head).boxed().await;
        if let Some(PageSerial(last_serial)) = page_serial(&response)
            && holds_below_readable_frontier(&state, state.index_at(position), last_serial)
        {
            response = not_found();
        }
        response
    };
    apply_revocation_cache_policy(&mut response, authenticated);
    response
}

/// The refusal to answer with when the index ACL does not grant this read, or `None` when it does.
///
/// The Simple API is a client protocol, so the `Authorization` header decides the request on its own
/// and a browser session does not stand in for a credential here, matching the upload and mutation
/// routes. An index left `anonymous_read` - the default - authorizes every caller, so a public index
/// answers exactly as it did.
fn read_denial(state: &ServingState, position: usize, rest: &str, headers: &HeaderMap) -> Option<Response> {
    let project = requested_project(rest);
    ReadAccess::from_headers(state, headers)
        .authorize_read(
            state,
            position,
            project.as_deref().map_or(ResourceMatch::Any, ResourceMatch::Pattern),
        )
        .err()
        .map(read_denial_response)
}

/// The project a `GET` addresses, so a token scoped to some of an index's projects reaches those and
/// no others.
///
/// A path this dispatch cannot read as a project is authorized as [`ResourceMatch::Any`]: the routes
/// that carry no project ask only whether the caller may read the index at all, and a malformed one
/// is refused on a closed index rather than answering its own `400` to a caller with no read.
fn requested_project(rest: &str) -> Option<String> {
    if let Ok(Some(target)) = legacy_json_target(rest) {
        return Some(target.project);
    }
    if let Ok(Some(project)) = simple_project(rest) {
        return Some(project.normalized);
    }
    let filename = ["files/", "inspect/"]
        .into_iter()
        .find_map(|prefix| rest.strip_prefix(prefix))?
        .split('/')
        .nth(1)?;
    safe_filename(filename)
        .ok()
        .map(|filename| crate::project_of_filename(&filename))
}

/// What a refused read is told. The answer is the same whether or not the resource exists, so a
/// caller without a read learns nothing about the index's contents from probing it.
fn read_denial_response(denial: Denial) -> Response {
    match denial {
        Denial::Forbidden => (StatusCode::FORBIDDEN, "credential does not grant this read").into_response(),
        // No credential can read an index that grants `read` to no token, but saying so would report
        // on its ACL; an unauthenticated caller gets the challenge either way.
        Denial::Unauthenticated | Denial::Unavailable => (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"peryx\"")],
            "unauthorized",
        )
            .into_response(),
    }
}

/// `PyPI` GET routing within an index: the Simple index and project detail (HTML, PEP 691 JSON, legacy
/// JSON), release files, and archive inspection.
///
/// Only the file route reads `head`. Everywhere else the answer is a page peryx has to produce anyway
/// to know its status, and axum drops the body of it; a file is the one representation whose body costs
/// an upstream download.
///
/// The streaming, resolve, and file branches each build a multi-kilobyte future (upstream fetch and
/// transform state). Inlined, they would size this dispatch future to their max and copy it on every
/// request, including the hot cached path that never enters them, so each is boxed to contribute a
/// pointer instead. See #779.
async fn pypi_get(
    state: &Arc<ServingState>,
    position: usize,
    rest: &str,
    headers: &HeaderMap,
    uri: &axum::http::Uri,
    head: bool,
) -> Response {
    let index = state.index_at(position);
    if let Some(response) = legacy_json_route(state, index, rest).boxed().await {
        return response;
    }
    if rest == "simple" {
        return simple_slash_redirect(uri, rest, "simple/");
    }
    if rest == "simple/" {
        return simple_index_response(state, index, headers);
    }
    let project = match simple_project(rest) {
        Ok(project) => project,
        Err(error) => return error.into_response(),
    };
    if let Some(project) = project {
        if !project.trailing_slash {
            return simple_slash_redirect(uri, rest, &format!("simple/{}/", project.normalized));
        }
        let Some(format) = negotiate(headers) else {
            return simple_not_acceptable_response();
        };
        let normalized = project.normalized;
        state.metrics.record(Observation::Page {
            repository: index.route.clone(),
            resource: normalized.clone(),
        });
        if matches!(format, Format::Json) {
            match cache::stream_detail(state.clone(), position, normalized.clone())
                .boxed()
                .await
            {
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
        if matches!(format, Format::Html) {
            let representation_key = state.representation_key(&index.route, &normalized, cache::SIMPLE_HTML);
            let hot = match revocation_safe_hot_page(state, &representation_key) {
                Ok(hot) => hot,
                Err(err) => return cache_error_response(&err, CacheContext::project(&index.route, &normalized)),
            };
            if let Some((bytes, last_serial)) = hot {
                return html_bytes_response(bytes, last_serial);
            }
            let detail = cache::resolve_detail_page(state, index, &normalized, &index.route)
                .boxed()
                .await;
            if let Ok(Some(found)) = &detail {
                let body = bytes::Bytes::from(crate::render_detail_html(&found.detail));
                remember_rendered(
                    state,
                    index,
                    &normalized,
                    cache::SIMPLE_HTML,
                    &body,
                    found.last_serial,
                    found.revoked_files_removed,
                );
                return html_bytes_response(body, found.last_serial);
            }
            return detail_response(detail, &index.route, &normalized);
        }
        let detail = cache::resolve_detail_page(state, index, &normalized, &index.route)
            .boxed()
            .await;
        return detail_response(detail, &index.route, &normalized);
    }
    if let Some(file) = rest.strip_prefix("files/") {
        return file_route(state, index, file, headers, head).boxed().await;
    }
    if let Some(target) = rest.strip_prefix("inspect/") {
        return inspect_route(state.clone(), position, target, uri.query())
            .boxed()
            .await;
    }
    not_found()
}

struct SimpleProject {
    normalized: String,
    trailing_slash: bool,
}

fn simple_project(rest: &str) -> HttpResult<Option<SimpleProject>> {
    let Some(project) = rest.strip_prefix("simple/") else {
        return Ok(None);
    };
    let trailing_slash = project.ends_with('/');
    let project = project.strip_suffix('/').unwrap_or(project);
    if project.is_empty() || project.contains('/') {
        return Ok(None);
    }
    let project = path::decode_path_segment(project).map_err(|error| path_error_response(&error))?;
    path::validate_path_segment("project", &project).map_err(|error| path_error_response(&error))?;
    if !crate::is_valid_name(&project) {
        return Ok(None);
    }
    Ok(Some(SimpleProject {
        normalized: normalize_name(&project),
        trailing_slash,
    }))
}

async fn legacy_json_route(state: &Arc<ServingState>, index: &Index, rest: &str) -> Option<Response> {
    let target = match legacy_json_target(rest) {
        Ok(Some(target)) => target,
        Ok(None) => return None,
        Err(error) => return Some(error.into_response()),
    };
    state.metrics.record(Observation::Page {
        repository: index.route.clone(),
        resource: target.project.clone(),
    });
    let variant = target.version.as_deref().map_or_else(
        || cache::LEGACY_JSON.to_owned(),
        |version| format!("{}/{version}", cache::LEGACY_JSON),
    );
    let representation_key = state.representation_key(&index.route, &target.project, &variant);
    let hot = match revocation_safe_hot_page(state, &representation_key) {
        Ok(hot) => hot,
        Err(err) => {
            return Some(cache_error_response(
                &err,
                CacheContext::project(&index.route, &target.project),
            ));
        }
    };
    if let Some((bytes, last_serial)) = hot {
        return Some(legacy_bytes_response(bytes, last_serial));
    }
    let detail = cache::resolve_detail_page(state, index, &target.project, &index.route)
        .boxed()
        .await;
    if let Ok(Some(found)) = &detail
        && let Some(body) = crate::legacy_json::render_legacy_json_with_serial(
            &found.detail,
            target.version.as_deref(),
            None,
            found.last_serial,
        )
    {
        let body = bytes::Bytes::from(body);
        remember_rendered(
            state,
            index,
            &target.project,
            &variant,
            &body,
            found.last_serial,
            found.revoked_files_removed,
        );
        return Some(legacy_bytes_response(body, found.last_serial));
    }
    Some(legacy_json_response(
        detail,
        &index.route,
        &target.project,
        target.version.as_deref(),
    ))
}

fn simple_index_response(state: &ServingState, index: &Index, headers: &HeaderMap) -> Response {
    let Some(format) = negotiate(headers) else {
        return simple_not_acceptable_response();
    };
    let list = cache::resolve_list(state, index)
        .and_then(|list| cache::list_serial(state, index).map(|last_serial| (list, last_serial)));
    index_response(list, format, &index.route)
}

fn simple_not_acceptable_response() -> Response {
    (
        StatusCode::NOT_ACCEPTABLE,
        [(header::VARY, "Accept")],
        "no acceptable Simple API representation",
    )
        .into_response()
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
    if let Err(err) = cache::ensure_digest_clear(state, &digest) {
        return cache_error_response(&err, CacheContext::file(&route, digest.as_str(), &filename));
    }
    match download_refusal(state, index, &filename, &digest).await {
        Ok(Some(refusal)) => return refusal.into_response(),
        Ok(None) => {}
        Err(err) => return cache_error_response(&err, CacheContext::file(&route, digest.as_str(), &filename)),
    }
    if filename.ends_with(".metadata") {
        state.metrics.record(Observation::Ecosystem {
            repository: route.clone(),
            resource: crate::project_of_filename(&filename),
            artifact: Some(filename.clone()),
            family: METADATA_FAMILY.key,
        });
        return match cache::metadata_bytes(state, index, &digest, &route, &filename).await {
            Ok(bytes) => validated_sidecar(metadata_response(bytes.clone()), &bytes, headers),
            Err(err) => cache_error_response(&err, CacheContext::metadata(&route, digest.as_str(), &filename)),
        };
    }
    if filename.ends_with(attestation::PROVENANCE_SUFFIX) {
        let artifact_filename = filename
            .strip_suffix(attestation::PROVENANCE_SUFFIX)
            .expect("suffix was checked");
        state.metrics.record(Observation::Ecosystem {
            repository: route.clone(),
            resource: crate::project_of_filename(&filename),
            artifact: Some(filename.clone()),
            family: PROVENANCE_FAMILY.key,
        });
        return match cache::provenance_bytes(state, index, &digest, artifact_filename)
            .boxed()
            .await
        {
            Ok(body) => {
                let bytes = body.bytes.clone();
                validated_sidecar(provenance_response(body), &bytes, headers)
            }
            Err(err) => cache_error_response(&err, CacheContext::provenance(&route, digest.as_str(), &filename)),
        };
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

/// Evaluates `If-None-Match` before method and range conditions, as required by RFC 9110 section 13.1.2.
///
/// Access and download-policy checks run first, so a match can return before opening or fetching the blob.
///
/// A digest this index has never cached matches all the same: the URL names the bytes, so a client
/// holding them holds the current representation whether or not the store does.
fn not_modified(headers: &HeaderMap, etag: &str) -> Option<Response> {
    matches_etag(headers, etag).then(|| unchanged(etag, None))
}

/// Whether the request holds `etag`.
///
/// `If-None-Match` is a list, so every field line has its say: a match in a later line answers the
/// request even when an earlier one named other bytes.
fn matches_etag(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|field| field.to_str().ok())
        .any(|field| if_none_match(field, etag))
}

/// Give a PEP 658 or PEP 740 sidecar a strong validator, and answer a matching conditional request
/// with the `304` it asked for.
///
/// The digest in the URL names the artifact, not its sidecar, so the tag is taken over the bytes
/// this request selected rather than off the path: a cached index refreshes an upstream provenance
/// document under `no-cache` and its content can change while the artifact's digest cannot. Both
/// sidecars are bounded and already in hand by this point, so the tag costs one hash of what is
/// about to be written to the socket.
///
/// Evaluating the condition after the representation is selected is also what the membership gate
/// wants: the download refusal has already run, so a `304` never answers for a pair this index does
/// not publish. See #1308.
///
/// The `304` is that same response with its body dropped, so the cache policy, media type and
/// provenance headers the `200` carried ride along - RFC 9111 s4.3.4 has a cache update its stored
/// response from them, and a `304` that re-stamped a proxied provenance document with the artifact
/// route's immutable policy would freeze a document upstream is still free to change. See #1309.
fn validated_sidecar(mut response: Response, body: &[u8], headers: &HeaderMap) -> Response {
    let etag = format!("\"{}\"", Digest::of(body).as_str());
    response.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(&etag).expect("a quoted hex digest is a valid header value"),
    );
    if matches_etag(headers, &etag) {
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        *response.body_mut() = axum::body::Body::empty();
    }
    response
}

/// The `If-Modified-Since` date this request leaves any say in, if it sent one.
///
/// RFC 9110 s13.1.3: an `If-None-Match` supersedes it, matched or not. A client that sent both asked
/// to be judged on the exact validator, and answering the date after the tag has already refused would
/// serve a `304` for bytes the client just said it does not hold.
///
/// The field carries one HTTP-date, not a list, so a second field line is malformed; the condition is
/// dropped rather than judged on an arbitrary line, since a `304` off a contradictory pair could
/// withhold bytes the client lacks.
fn conditional_date(headers: &HeaderMap) -> Option<&str> {
    if headers.contains_key(header::IF_NONE_MATCH) {
        return None;
    }
    let mut dates = headers.get_all(header::IF_MODIFIED_SINCE).iter();
    let date = dates.next()?;
    if dates.next().is_some() {
        return None;
    }
    date.to_str().ok()
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

/// The gate every route that releases an artifact's bytes runs first: the project's stored status,
/// then its membership of this index, then the index's serve policy.
///
/// Archive inspection and the UI archive browser share it with the file route, so quarantining a
/// project or denying its files at [`PolicyAction::Serve`] cannot be walked around by asking for an
/// archive's members instead of its bytes. See #1524.
///
/// Membership is a [`cache::CacheError::FileNotFound`], which is what every caller already answers
/// a digest no index knows with, so a pair another index published is refused in the same bytes as
/// one nothing published: the routes cannot become an existence oracle for a private artifact.
/// See #1308.
///
/// The order is what each gate can see. A quarantine withholds the project's files from the page
/// membership reads, so status runs first or a quarantine would answer as a missing file rather
/// than with its own refusal. Membership then runs ahead of policy, which reads the size of
/// whatever blob the digest names: a caller who pairs a foreign digest with a filename of their
/// choosing gets neither a policy decision made on that name nor the size behind that digest.
pub(super) async fn download_refusal(
    state: &ServingState,
    index: &Index,
    filename: &str,
    digest: &Digest,
) -> Result<Option<DownloadRefusal>, cache::CacheError> {
    let status = cache::download_status(state, index, filename)?;
    if !status.offers_downloads() {
        return Ok(Some(DownloadRefusal::withheld(filename, status)));
    }
    if !cache::publishes_file(state, index, filename, digest)? {
        return Err(cache::CacheError::FileNotFound);
    }
    Ok(download_policy_denial(state, index, filename, digest)
        .await?
        .as_ref()
        .map(DownloadRefusal::policy))
}

async fn download_policy_denial(
    state: &ServingState,
    index: &Index,
    filename: &str,
    digest: &Digest,
) -> Result<Option<PolicyDenial>, cache::CacheError> {
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
    Ok(index.policy.check_download(PolicyAction::Serve, filename, size).err())
}

struct LegacyJsonTarget {
    project: String,
    version: Option<String>,
}

fn legacy_json_target(rest: &str) -> HttpResult<Option<LegacyJsonTarget>> {
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

/// Answer a file `HEAD` with the headers of the `GET` it stands for and no body.
///
/// Nothing here opens the artifact or asks upstream for it, which is the point: a probe of an uncached
/// wheel used to start the whole download - hashed, written, and paid for in bandwidth - for a client
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
            match parse_range(range, size) {
                RangeRequest::Whole => (StatusCode::OK, Some(size), None, modified),
                RangeRequest::Unsatisfiable => return unsatisfiable_range(size),
                RangeRequest::Partial(range) => (
                    StatusCode::PARTIAL_CONTENT,
                    Some(range.end - range.start),
                    Some(format!("bytes {}-{}/{size}", range.start, range.end - 1)),
                    modified,
                ),
            }
        }
        // An uncached blob has no write to date, the way the teed GET has none to state.
        cache::FileProbe::Upstream(size) => (StatusCode::OK, size, None, None),
    };
    let mut builder = Response::builder().status(status);
    for (name, value) in [
        (header::CONTENT_TYPE, "application/octet-stream"),
        (header::CACHE_CONTROL, IMMUTABLE),
        (header::ACCEPT_RANGES, "bytes"),
        (header::ETAG, etag),
    ] {
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
/// which side of the cache serves them. A blob still arriving from upstream has no such date - the
/// write it would name has not happened - so it goes out with the tag alone, as it did before.
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
    let blob_headers = [
        (header::CONTENT_TYPE, "application/octet-stream"),
        (header::CACHE_CONTROL, IMMUTABLE),
        (header::ACCEPT_RANGES, "bytes"),
        (header::ETAG, etag),
    ];
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
            let (status, start, length, content_range) = match parse_range(range, size) {
                RangeRequest::Whole => (StatusCode::OK, 0, size, None),
                RangeRequest::Unsatisfiable => return unsatisfiable_range(size),
                RangeRequest::Partial(range) => (
                    StatusCode::PARTIAL_CONTENT,
                    range.start,
                    range.end - range.start,
                    Some(format!("bytes {}-{}/{size}", range.start, range.end - 1)),
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
            let (version, source) = cache::download_dimensions(state, &digest, filename);
            let filename = filename.to_owned();
            let body =
                peryx_driver::body::on_body_complete(peryx_driver::body::blob_read(read), length, move |bytes| {
                    metrics.record(Observation::Read {
                        resource: project,
                        repository: route,
                        artifact: filename,
                        group: version,
                        source,
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
    revoked_files_removed: bool,
) {
    if revoked_files_removed {
        return;
    }
    if let Ok(Some(expires_at)) = cache::rendered_expiry(state, index, project) {
        let key = state.representation_key(&index.route, project, variant);
        state
            .cache
            .store_hot_versioned(key, body.clone(), expires_at, last_serial);
    }
}

fn revocation_safe_hot_page(
    state: &ServingState,
    key: &str,
) -> Result<Option<(bytes::Bytes, Option<u64>)>, CacheError> {
    if cache::has_active_revocations(state)? {
        return Ok(None);
    }
    Ok(state.hot_fresh_versioned(key))
}

fn apply_revocation_cache_policy(response: &mut Response, authenticated: bool) {
    if response
        .headers()
        .get(header::CACHE_CONTROL)
        .is_some_and(|value| value == "no-cache")
    {
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(if authenticated {
                "private, no-cache"
            } else {
                "public, no-cache"
            }),
        );
        return;
    }
    // RFC 9111 s4.3.4 updates the stored response with the `304`'s header fields, so a `304` keeps the
    // policy of the `200` it validated; `no-store` there would make a cache drop the artifact it just
    // revalidated and pull the immutable bytes again.
    let value = if response.status().is_success() || response.status() == StatusCode::NOT_MODIFIED {
        format!(
            "{}, max-age={}, must-revalidate, no-transform",
            if authenticated { "private" } else { "public" },
            peryx_driver::revocations::DECISION_CACHE_TTL_SECS,
        )
    } else {
        "no-store".to_owned()
    };
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_str(&value).expect("cache policy is a valid header"),
    );
}
