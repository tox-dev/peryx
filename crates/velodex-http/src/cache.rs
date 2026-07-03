//! The read-through cache and index composition: serve a project's simple page and file bytes across
//! an index's layers, fetching and caching from upstream on a miss.

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use velodex_core::pypi::file_matches_version;
use velodex_core::pypi::{
    CoreMetadata, File, Meta, ProjectDetail, ProjectList, ProjectListEntry, Yanked, parse_detail, parse_detail_html,
    to_json,
};
use velodex_storage::blob::Digest;
use velodex_storage::meta::CachedIndex;
use velodex_upstream::{SimpleResponse, UpstreamClient};

use crate::metrics::Event;
use crate::path_safety::local_file_url;
use crate::state::{AppState, Index, IndexKind};
use crate::stream::{PageSummary, PageTransformer};
use crate::upload::{PreparedUpload, Uploaded};

/// An error while producing a cached response.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error(transparent)]
    Meta(#[from] velodex_storage::meta::MetaError),
    #[error(transparent)]
    Blob(#[from] velodex_storage::blob::BlobError),
    #[error(transparent)]
    Upstream(#[from] velodex_upstream::UpstreamError),
    #[error(transparent)]
    Parse(#[from] serde_json::Error),
    #[error("upstream unreachable and nothing cached")]
    Unavailable,
    #[error("index is not volatile; delete is disabled")]
    NotVolatile,
    #[error("no known source for this file")]
    FileNotFound,
    #[error("file already exists: {0}")]
    FileExists(String),
    #[error("file stream failed: {0}")]
    Stream(String),
}

/// Resolve a project's detail on `index`, composing overlay layers.
///
/// Every file URL is rewritten to `serve_route` so clients fetch through the route they asked on;
/// returns `None` when no layer has the project.
///
/// # Errors
/// Returns [`CacheError`] on a store, parse, or (with no cached fallback) upstream error.
pub async fn resolve_detail(
    state: &AppState,
    index: &Index,
    project: &str,
    serve_route: &str,
) -> Result<Option<ProjectDetail>, CacheError> {
    match &index.kind {
        IndexKind::Mirror(client) => {
            let Some(mut detail) = mirror_detail(state, &index.name, &index.route, client, project).await? else {
                return Ok(None);
            };
            rewrite_urls(&mut detail, serve_route);
            Ok(Some(detail))
        }
        IndexKind::Local { .. } => {
            let Some(mut detail) = local_detail(state, &index.name, project)? else {
                return Ok(None);
            };
            rewrite_urls(&mut detail, serve_route);
            Ok(Some(detail))
        }
        IndexKind::Overlay { layers, upload } => overlay_detail(state, layers, *upload, project, serve_route).await,
    }
}

/// Merge the layers of an overlay: first match per filename wins, versions are unioned. Overrides
/// recorded on the overlay's upload layer then apply: `hidden` files drop out of the page and
/// `yanked` files carry the PEP 592 marker, which is how read-only upstream files are yanked or
/// removed without touching the mirror.
async fn overlay_detail(
    state: &AppState,
    layers: &[usize],
    upload: Option<usize>,
    project: &str,
    serve_route: &str,
) -> Result<Option<ProjectDetail>, CacheError> {
    // Layers resolve concurrently; the merge below preserves their configured precedence.
    let resolved = futures_util::future::join_all(layers.iter().map(|&pos| {
        let layer = state.index_at(pos);
        Box::pin(resolve_detail(state, layer, project, serve_route))
    }))
    .await;
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    let mut versions = BTreeSet::new();
    let mut found = false;
    for (&pos, outcome) in layers.iter().zip(resolved) {
        // A layer being unavailable (a down mirror with a cold cache) must not break the others.
        let detail = match outcome {
            Ok(detail) => detail,
            Err(err) => {
                let layer = state.index_at(pos);
                tracing::warn!(layer = %layer.name, error = ?err, "overlay layer unavailable, skipping");
                continue;
            }
        };
        if let Some(detail) = detail {
            found = true;
            versions.extend(detail.versions);
            for file in detail.files {
                if seen.insert(file.filename.clone()) {
                    files.push(file);
                }
            }
        }
    }
    if !found {
        return Ok(None);
    }
    if let Some(pos) = upload {
        apply_overrides(state, &state.index_at(pos).name, project, &mut files)?;
    }
    Ok(Some(ProjectDetail {
        meta: Meta::default(),
        name: project.to_owned(),
        versions: versions.into_iter().collect(),
        files,
    }))
}

/// Apply the `hidden`/`yanked` overrides stored on `local` to a merged file list.
fn apply_overrides(state: &AppState, local: &str, project: &str, files: &mut Vec<File>) -> Result<(), CacheError> {
    let overrides: std::collections::HashMap<String, String> =
        state.meta.list_overrides(local, project)?.into_iter().collect();
    if overrides.is_empty() {
        return Ok(());
    }
    files.retain(|file| overrides.get(&file.filename).map(String::as_str) != Some("hidden"));
    for file in files {
        if overrides.get(&file.filename).map(String::as_str) == Some("yanked") {
            file.yanked = Yanked::Yes;
        }
    }
    Ok(())
}

/// Fetch a mirror's project detail, serving from cache when fresh and revalidating or fetching
/// otherwise. Returns `None` when the project does not exist upstream.
///
/// Concurrent misses for the same page are single-flighted: resolvers such as uv request one
/// project several times in parallel, and each duplicate fetch would download and store a
/// multi-megabyte page again.
async fn mirror_detail(
    state: &AppState,
    name: &str,
    route: &str,
    client: &UpstreamClient,
    project: &str,
) -> Result<Option<ProjectDetail>, CacheError> {
    let key = format!("{name}/{project}");
    if let Some(record) = fresh_cached(state, &key)? {
        return Ok(Some(raw_to_detail(route, &record)?));
    }

    let gate = flight_gate(state, &key);
    let _guard = gate.lock().await;
    // Whoever held the gate first has stored the page by now; everyone else serves it from cache.
    if let Some(record) = fresh_cached(state, &key)? {
        return Ok(Some(raw_to_detail(route, &record)?));
    }

    let result = fetch_and_store(state, &key, name, project, client).await;
    state.inflight.lock().expect("inflight lock").remove(&key);
    match result? {
        Some(record) => Ok(Some(raw_to_detail(route, &record)?)),
        None => Ok(None),
    }
}

/// The per-page lock concurrent cache misses share.
pub(crate) fn flight_gate(state: &AppState, key: &str) -> Arc<tokio::sync::Mutex<()>> {
    let mut inflight = state.inflight.lock().expect("inflight lock");
    inflight.entry(key.to_owned()).or_default().clone()
}

/// Release a single-flight hold: unlock first so a waiter parked on the gate proceeds, then drop
/// the map entry so later requests start fresh.
fn release_flight(state: &AppState, key: &str, guard: tokio::sync::OwnedMutexGuard<()>) {
    drop(guard);
    state.inflight.lock().expect("inflight lock").remove(key);
}

/// The cached raw page, when it is still within its freshness window: upstream's `Cache-Control`
/// lifetime when it granted one, the configured fallback otherwise.
pub(crate) fn fresh_cached(state: &AppState, key: &str) -> Result<Option<CachedIndex>, CacheError> {
    let now = (state.clock)();
    match state.meta.get_index(key)? {
        Some(record) if now - record.fetched_at_unix < freshness(state, &record) => Ok(Some(record)),
        _ => Ok(None),
    }
}

/// A record's freshness lifetime in seconds.
fn freshness(state: &AppState, record: &CachedIndex) -> i64 {
    record.fresh_secs.unwrap_or(state.ttl_secs)
}

/// Fetch a page (buffered) and persist the raw body plus all file registrations in one transaction.
/// Used by the non-streaming path: HTML upstreams, HTML clients, and internal consumers.
///
/// Every outcome that a log line describes also lands in the metrics tree: revalidations (and
/// whether upstream actually changed), stale fallbacks, and hard upstream failures.
async fn fetch_and_store(
    state: &AppState,
    key: &str,
    name: &str,
    project: &str,
    client: &UpstreamClient,
) -> Result<Option<CachedIndex>, CacheError> {
    let now = (state.clock)();
    let cached = state.meta.get_index(key)?;
    let etag = cached.as_ref().and_then(|record| record.etag.clone());
    let route = mirror_route(state, name);
    let event_project = project.to_owned();
    match client.fetch_project(project, etag.as_deref()).await {
        Ok(response) if response.status == 200 => {
            let record = CachedIndex {
                etag: response.etag.clone(),
                last_serial: response.last_serial,
                fetched_at_unix: now,
                content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
                fresh_secs: response.max_age,
                body: canonical_raw(project, &response),
            };
            if let Some(previous) = &cached {
                let changed = previous.body != record.body;
                if changed {
                    tracing::info!(%key, "upstream page changed");
                }
                let event = Event::Refresh {
                    route,
                    project: event_project,
                    changed,
                };
                state.metrics.record(event);
            }
            persist_page(state, key, name, project, &record)?;
            Ok(Some(record))
        }
        Ok(response) if response.status == 304 => {
            let mut record = cached.ok_or(CacheError::Unavailable)?;
            record.fetched_at_unix = now;
            record.fresh_secs = response.max_age.or(record.fresh_secs);
            state.meta.put_index(key, &record)?;
            state.metrics.record(Event::Refresh {
                route,
                project: event_project,
                changed: false,
            });
            Ok(Some(record))
        }
        Ok(response) if response.status == 404 => Ok(None),
        Ok(response) => cached.map_or_else(
            || {
                state.metrics.record(Event::UpstreamError {
                    route: route.clone(),
                    project: event_project.clone(),
                });
                Err(CacheError::Unavailable)
            },
            |record| {
                tracing::warn!(%key, status = response.status, "upstream errored; serving stale page");
                state.metrics.record(Event::StaleServed {
                    route: route.clone(),
                    project: event_project.clone(),
                });
                Ok(Some(record))
            },
        ),
        Err(err) => cached.map_or_else(
            || {
                state.metrics.record(Event::UpstreamError {
                    route: route.clone(),
                    project: event_project.clone(),
                });
                Err(CacheError::Upstream(err))
            },
            |record| {
                tracing::warn!(%key, "upstream unreachable; serving stale page");
                state.metrics.record(Event::StaleServed {
                    route: route.clone(),
                    project: event_project.clone(),
                });
                Ok(Some(record))
            },
        ),
    }
}

/// The route a mirror's cached pages are attributed to in metrics.
fn mirror_route(state: &AppState, name: &str) -> String {
    state
        .indexes
        .iter()
        .find(|index| index.name == name)
        .map(|index| index.route.clone())
        .expect("events are recorded only for resolved mirrors")
}

/// One background refresh sweep's outcome.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RefreshSummary {
    /// Stale pages revalidated against upstream.
    pub checked: usize,
    /// Pages whose upstream content differed from the cache.
    pub changed: usize,
}

/// Revalidate every cached mirror page older than the TTL.
///
/// Upstream changes are caught within one refresh period even for pages nobody is requesting.
/// Pages run sequentially: a large cache trickles out as cheap conditional requests (`ETag` hits
/// answer 304 with no body) instead of a burst against upstream. Each revalidation is logged and
/// counted through the same events as the on-demand path.
///
/// # Errors
/// Returns [`CacheError`] when the local store fails; upstream failures do not error (a page with
/// a cached copy serves stale and is retried next sweep).
pub async fn refresh_stale_pages(state: &Arc<AppState>) -> Result<RefreshSummary, CacheError> {
    let now = (state.clock)();
    let mut summary = RefreshSummary::default();
    for (key, fetched_at, fresh_secs) in state.meta.list_index_pages()? {
        if now - fetched_at < fresh_secs.unwrap_or(state.ttl_secs) {
            continue;
        }
        let Some((index, client, project)) = mirror_for_key(state, &key) else {
            continue;
        };
        summary.checked += 1;
        let before = state.meta.get_index(&key)?.map(|record| record.body);
        if let Some(record) = fetch_and_store(state, &key, &index.name, &project, client).await?
            && before.as_ref() != Some(&record.body)
        {
            summary.changed += 1;
        }
    }
    Ok(summary)
}

/// Map a cache key (`{mirror name}/{project}`) back to its mirror and client; the longest matching
/// name wins when one mirror's name prefixes another's.
fn mirror_for_key<'a>(state: &'a AppState, key: &str) -> Option<(&'a Index, &'a UpstreamClient, String)> {
    state
        .indexes
        .iter()
        .filter_map(|index| match &index.kind {
            IndexKind::Mirror(client) => {
                let project = key.strip_prefix(&index.name)?.strip_prefix('/')?;
                Some((index, client, project.to_owned()))
            }
            IndexKind::Local { .. } | IndexKind::Overlay { .. } => None,
        })
        .max_by_key(|(index, _, _)| index.name.len())
}

/// The canonical raw body to persist: JSON pages verbatim, HTML pages converted once to PEP 691
/// JSON (with upstream URLs intact), so every later read has one format to deal with.
fn canonical_raw(project: &str, response: &SimpleResponse) -> Vec<u8> {
    if is_json(response.content_type.as_deref()) {
        return response.body.to_vec();
    }
    let parsed = parse_detail_html(project, &String::from_utf8_lossy(&response.body), &response.url);
    let detail = ProjectDetail {
        meta: Meta::default(),
        name: parsed.name,
        versions: parsed.versions,
        files: parsed.files,
    };
    to_json(&detail).into_bytes()
}

pub(crate) fn is_json(content_type: Option<&str>) -> bool {
    // Legacy records carry no content type and hold JSON documents.
    content_type.is_none_or(|content_type| content_type.contains("json"))
}

/// Persist a raw page and everything derived from it in one transaction: the page record, the
/// project name, and every file's source URL and PEP 658 sibling.
/// Persist a streamed page from what the transformer already extracted: no re-parse of the raw
/// body sits on the serving path, which a serial client feels on every large cold page.
fn persist_streamed(
    state: &AppState,
    key: &str,
    name: &str,
    project: &str,
    record: &CachedIndex,
    summary: &PageSummary,
) -> Result<(), CacheError> {
    let files: Vec<(String, String)> = summary
        .registrations
        .iter()
        .map(|registration| (registration.sha256.clone(), registration.url.clone()))
        .collect();
    let metadata: Vec<(String, String, String)> = summary
        .registrations
        .iter()
        .filter_map(|registration| {
            let (url, digest) = registration.metadata.as_ref()?;
            Some((registration.sha256.clone(), url.clone(), digest.clone()))
        })
        .collect();
    let display = summary.name.as_deref().unwrap_or(project);
    state
        .meta
        .put_mirror_page(key, record, name, project, display, name, &files, &metadata)?;
    state.bump_epoch();
    Ok(())
}

pub(crate) fn persist_page(
    state: &AppState,
    key: &str,
    name: &str,
    project: &str,
    record: &CachedIndex,
) -> Result<(), CacheError> {
    let parsed = parse_detail(&record.body)?;
    let mut files = Vec::new();
    let mut metadata = Vec::new();
    for file in &parsed.files {
        let Some(sha256) = file.hashes.get("sha256") else {
            continue;
        };
        if file.url.starts_with('/') {
            continue; // a legacy record with velodex-route URLs has nothing to register
        }
        files.push((sha256.clone(), file.url.clone()));
        if let CoreMetadata::Hashes(hashes) = &file.core_metadata
            && let Some(digest) = hashes.get("sha256")
        {
            metadata.push((sha256.clone(), format!("{}.metadata", file.url), digest.clone()));
        }
    }
    let display = if parsed.name.is_empty() { project } else { &parsed.name };
    state
        .meta
        .put_mirror_page(key, record, name, project, display, name, &files, &metadata)?;
    state.bump_epoch();
    Ok(())
}

/// Turn a raw cached page into the detail served on `route`: parse, drop unverifiable metadata
/// claims, and point content-addressable files at velodex's own file route.
pub(crate) fn raw_to_detail(route: &str, record: &CachedIndex) -> Result<ProjectDetail, CacheError> {
    let parsed = parse_detail(&record.body)?;
    let files = parsed.files.into_iter().map(|file| present_file(file, route)).collect();
    Ok(ProjectDetail {
        meta: Meta::default(),
        name: parsed.name,
        versions: parsed.versions,
        files,
    })
}

/// The pure serving transform for one file: velodex URL for content-addressable files, metadata
/// claims kept only when verifiable by digest.
fn present_file(mut file: File, route: &str) -> File {
    let Some(sha256) = file.hashes.get("sha256") else {
        file.core_metadata = CoreMetadata::Absent;
        return file;
    };
    if !matches!(&file.core_metadata, CoreMetadata::Hashes(hashes) if hashes.contains_key("sha256")) {
        file.core_metadata = CoreMetadata::Absent;
    }
    if !file.url.starts_with('/') {
        file.url = local_file_url(route, sha256, &file.filename);
    }
    file
}

/// Build a local (uploaded) project's detail from its stored file records. Yank markers are kept, so
/// yanked files stay downloadable but are skipped by resolvers.
fn local_detail(state: &AppState, name: &str, project: &str) -> Result<Option<ProjectDetail>, CacheError> {
    let entries = state.meta.list_upload_entries(name, project)?;
    if entries.is_empty() {
        return Ok(None);
    }
    let mut files = Vec::with_capacity(entries.len());
    let mut versions = BTreeSet::new();
    for (_filename, bytes) in entries {
        let uploaded: Uploaded = serde_json::from_slice(&bytes)?;
        versions.insert(uploaded.version);
        files.push(uploaded.file);
    }
    Ok(Some(ProjectDetail {
        meta: Meta::default(),
        name: project.to_owned(),
        versions: versions.into_iter().collect(),
        files,
    }))
}

/// Point every content-addressable file at velodex's own file route on `route`.
fn rewrite_urls(detail: &mut ProjectDetail, route: &str) {
    for file in &mut detail.files {
        if let Some(sha256) = file.hashes.get("sha256") {
            file.url = local_file_url(route, sha256, &file.filename);
        }
    }
}

/// The project names velodex has observed on `index`, unioned across an overlay's layers.
///
/// # Errors
/// Returns [`CacheError`] if a store read fails.
pub fn resolve_list(state: &AppState, index: &Index) -> Result<ProjectList, CacheError> {
    let mut names = BTreeSet::new();
    collect_projects(state, index, &mut names)?;
    Ok(ProjectList {
        meta: Meta::default(),
        projects: names.into_iter().map(|name| ProjectListEntry { name }).collect(),
    })
}

fn collect_projects(state: &AppState, index: &Index, names: &mut BTreeSet<String>) -> Result<(), CacheError> {
    match &index.kind {
        IndexKind::Mirror(_) | IndexKind::Local { .. } => {
            names.extend(state.meta.list_projects(&index.name)?);
        }
        IndexKind::Overlay { layers, .. } => {
            for &pos in layers {
                collect_projects(state, state.index_at(pos), names)?;
            }
        }
    }
    Ok(())
}

/// Fetch a URL through the named mirror's client (reusing its authentication).
async fn fetch_from_source(state: &AppState, source: &str, url: &str) -> Result<Bytes, CacheError> {
    Ok(source_client(state, source)?.fetch_bytes(url).await?)
}

/// Resolve a file to a local blob path. A cache miss is fetched through the same streaming path as
/// downloads, so the archive inspector never buffers the whole artifact in memory.
///
/// # Errors
/// Returns [`CacheError::FileNotFound`] if the digest has no known source, or another error on a
/// store or upstream failure.
///
/// # Panics
/// Panics only if the downloads registry lock is poisoned by an earlier panic.
#[expect(
    clippy::significant_drop_tightening,
    reason = "the connect gate stays held across start_download so racing inspectors attach instead of double-fetching"
)]
pub async fn file_path(
    state: Arc<AppState>,
    digest: Digest,
    route: String,
    filename: String,
) -> Result<PathBuf, CacheError> {
    if state.blobs.exists(&digest) {
        return Ok(state.blobs.path_for(&digest));
    }
    let gate = flight_gate(&state, digest.as_str());
    let guard = gate.lock_owned().await;
    if state.blobs.exists(&digest) {
        release_flight(&state, digest.as_str(), guard);
        return Ok(state.blobs.path_for(&digest));
    }
    let mut handle = if let Some(running) = existing_download(&state, &digest) {
        running
    } else {
        let started = start_download(&state, &digest, route, filename).await?;
        state
            .downloads
            .lock()
            .expect("downloads lock")
            .insert(digest.as_str().to_owned(), started.clone());
        started
    };
    release_flight(&state, digest.as_str(), guard);
    wait_for_download(&mut handle).await?;
    Ok(state.blobs.path_for(&digest))
}

/// Resolve a wheel's PEP 658 metadata bytes: cached blob, or fetch the sibling from its source
/// mirror, verify against the advertised digest, and cache.
///
/// # Errors
/// Returns [`CacheError::FileNotFound`] if the wheel has no known metadata sibling, or another error
/// on a store or upstream failure.
pub async fn metadata_bytes(state: &AppState, wheel_digest: &Digest) -> Result<Bytes, CacheError> {
    let (url, metadata_hex, source) = state
        .meta
        .get_metadata(wheel_digest.as_str())?
        .ok_or(CacheError::FileNotFound)?;
    let metadata_digest = Digest::from_hex(&metadata_hex).ok_or(CacheError::FileNotFound)?;
    if state.blobs.exists(&metadata_digest) {
        return Ok(Bytes::from(state.blobs.read(&metadata_digest)?));
    }
    let bytes = fetch_from_source(state, &source, &url).await?;
    state.blobs.write_verified(&bytes, &metadata_digest)?;
    Ok(bytes)
}

/// Persist a prepared upload into the local store `name`: commit the staged blob, record the file
/// and its project, and bump the serial. Returns `false` for a same-bytes duplicate.
///
/// # Errors
/// Returns [`CacheError`] if a blob write, store write, or encode fails.
pub fn store_upload(state: &AppState, name: &str, prepared: PreparedUpload) -> Result<bool, CacheError> {
    let PreparedUpload {
        normalized,
        display_name,
        filename,
        digest: content_digest,
        content,
        metadata,
        record,
    } = prepared;
    if let Some(existing) = state.meta.get_upload(name, &normalized, &filename)? {
        let uploaded: Uploaded = serde_json::from_slice(&existing)?;
        if uploaded
            .file
            .hashes
            .get("sha256")
            .is_some_and(|hash| hash == content_digest.as_str())
        {
            state.blobs.commit_staged(content)?;
            return Ok(false);
        }
        return Err(CacheError::FileExists(filename));
    }
    state.blobs.commit_staged(content)?;
    let mut record = record;
    if let Some(metadata) = metadata {
        let metadata_digest = state.blobs.write(&metadata)?;
        state
            .meta
            .put_metadata(content_digest.as_str(), "uploaded", metadata_digest.as_str(), name)?;
        record.file.core_metadata = CoreMetadata::Hashes(std::collections::BTreeMap::from([(
            "sha256".to_owned(),
            metadata_digest.as_str().to_owned(),
        )]));
    }
    let record = to_json(&record).into_bytes();
    state.meta.put_upload(name, &normalized, &filename, &record)?;
    state.meta.put_project(name, &normalized, &display_name)?;
    state.meta.next_serial()?;
    state.bump_epoch();
    Ok(true)
}

/// The two reversible override kinds for files served from read-only layers.
const YANKED: &str = "yanked";
const HIDDEN: &str = "hidden";

/// Set or clear the yank state of a project's files as served by `index`.
///
/// Uploaded files get their stored record rewritten; read-only upstream files get a `yanked`
/// override on `local`. Returns how many files changed.
///
/// # Errors
/// Returns [`CacheError`] on a store, decode, or resolution failure.
pub async fn set_yanked(
    state: &AppState,
    index: &Index,
    local: &str,
    normalized: &str,
    version: Option<&str>,
    yanked: bool,
) -> Result<usize, CacheError> {
    let uploaded = upload_filenames(state, local, normalized)?;
    let mut changed = yank_uploads(
        state,
        local,
        normalized,
        version,
        &if yanked { Yanked::Yes } else { Yanked::No },
    )?;
    for filename in served_filenames(state, index, normalized, version).await? {
        if uploaded.contains(&filename) {
            continue;
        }
        if yanked {
            state.meta.put_override(local, normalized, &filename, YANKED)?;
            changed += 1;
        } else if state.meta.delete_override(local, normalized, &filename)? {
            changed += 1;
        }
    }
    if changed > 0 {
        state.bump_epoch();
    }
    Ok(changed)
}

/// Remove a project's files as served by `index`.
///
/// Uploaded files are deleted outright (requires `volatile`); read-only upstream files get a
/// reversible `hidden` override on `local`. Returns how many files were affected.
///
/// # Errors
/// Returns [`CacheError::NotVolatile`] when uploaded files match but the local store is not
/// volatile, or another [`CacheError`] on a store or resolution failure.
pub async fn remove_files(
    state: &AppState,
    index: &Index,
    local: &str,
    volatile: bool,
    normalized: &str,
    version: Option<&str>,
) -> Result<usize, CacheError> {
    let uploaded = upload_filenames(state, local, normalized)?;
    let mut affected = 0;
    let mut matched_upload = false;
    for filename in served_filenames(state, index, normalized, version).await? {
        if uploaded.contains(&filename) {
            matched_upload = true;
            if !volatile {
                return Err(CacheError::NotVolatile);
            }
            if state.meta.delete_upload(local, normalized, &filename)? {
                affected += 1;
            }
        } else {
            state.meta.put_override(local, normalized, &filename, HIDDEN)?;
            affected += 1;
        }
    }
    // A versioned delete whose filenames carry no parsable version misses the served-page filter;
    // fall back to matching the version stored in the upload records. A project-level delete never
    // needs this: every upload is on the served page.
    if !matched_upload && let Some(version) = version {
        affected += delete_uploads_of_version(state, local, normalized, version)?;
    }
    if affected > 0 {
        state.bump_epoch();
    }
    Ok(affected)
}

/// Clear `hidden` overrides for a project (optionally one version), restoring upstream files that a
/// delete removed from the merged page. Returns how many files reappeared.
///
/// # Errors
/// Returns [`CacheError`] on a store failure.
pub fn restore_files(
    state: &AppState,
    local: &str,
    normalized: &str,
    version: Option<&str>,
) -> Result<usize, CacheError> {
    let mut restored = 0;
    for (filename, kind) in state.meta.list_overrides(local, normalized)? {
        if kind != HIDDEN {
            continue;
        }
        if version.is_some_and(|version| !file_matches_version(&filename, version)) {
            continue;
        }
        if state.meta.delete_override(local, normalized, &filename)? {
            restored += 1;
        }
    }
    if restored > 0 {
        state.bump_epoch();
    }
    Ok(restored)
}

/// The filenames the serving index currently shows for a project, filtered to one version when
/// given. Hidden files are resolved too (the page-level filter does not apply here), so a delete
/// followed by a delete stays idempotent rather than erroring.
async fn served_filenames(
    state: &AppState,
    index: &Index,
    normalized: &str,
    version: Option<&str>,
) -> Result<Vec<String>, CacheError> {
    let Some(detail) = Box::pin(resolve_detail(state, index, normalized, &index.route)).await? else {
        return Ok(Vec::new());
    };
    Ok(detail
        .files
        .into_iter()
        .map(|file| file.filename)
        .filter(|filename| version.is_none_or(|version| file_matches_version(filename, version)))
        .collect())
}

fn upload_filenames(state: &AppState, local: &str, normalized: &str) -> Result<HashSet<String>, CacheError> {
    Ok(state
        .meta
        .list_upload_entries(local, normalized)?
        .into_iter()
        .map(|(filename, _)| filename)
        .collect())
}

/// Delete the uploaded file records whose stored version matches. Returns how many were removed.
fn delete_uploads_of_version(
    state: &AppState,
    name: &str,
    normalized: &str,
    version: &str,
) -> Result<usize, CacheError> {
    let mut removed = 0;
    for (filename, bytes) in state.meta.list_upload_entries(name, normalized)? {
        let uploaded: Uploaded = serde_json::from_slice(&bytes)?;
        if uploaded.version == version && state.meta.delete_upload(name, normalized, &filename)? {
            removed += 1;
        }
    }
    Ok(removed)
}

/// Set the yank state of uploaded files, optionally limited to one version. Returns how many
/// changed.
fn yank_uploads(
    state: &AppState,
    name: &str,
    normalized: &str,
    version: Option<&str>,
    yanked: &Yanked,
) -> Result<usize, CacheError> {
    let mut changed = 0;
    for (filename, bytes) in state.meta.list_upload_entries(name, normalized)? {
        let mut uploaded: Uploaded = serde_json::from_slice(&bytes)?;
        if version.is_some_and(|version| uploaded.version != version) {
            continue;
        }
        if uploaded.file.yanked == *yanked {
            continue;
        }
        uploaded.file.yanked = yanked.clone();
        state
            .meta
            .put_upload(name, normalized, &filename, &to_json(&uploaded).into_bytes())?;
        changed += 1;
    }
    Ok(changed)
}

/// How a simple-page request gets its bytes.
pub enum PageOutcome {
    /// The full transformed document, from the hot cache or a warm raw page.
    Ready(Bytes),
    /// A live upstream fetch, transformed chunk by chunk as it arrives. The raw body tees into the
    /// page cache and the transformed body into the hot cache when the stream completes.
    Streaming(futures_util::stream::BoxStream<'static, Result<Bytes, std::io::Error>>),
    /// The project does not exist upstream.
    NotFound,
    /// Not streamable here (several mirror layers, or no mirror); the buffered path serves it.
    Fallback,
}

/// Serve a simple page with maximum overlap: hot cache first, then a warm raw page transformed in
/// memory, then a streaming upstream fetch.
///
/// # Errors
/// Returns [`CacheError`] on a store failure; upstream failures degrade to [`PageOutcome::Fallback`]
/// so the buffered path can serve stale data.
#[allow(
    clippy::significant_drop_tightening,
    reason = "the flight guard deliberately lives until it moves into the stream or is released"
)]
pub async fn stream_detail(state: Arc<AppState>, position: usize, project: String) -> Result<PageOutcome, CacheError> {
    let index = state.index_at(position);
    let route = index.route.clone();
    let Some((mirror_name, client, context)) = streaming_parts(&state, index, &project)? else {
        return Ok(PageOutcome::Fallback);
    };

    let hot_key = state.hot_key(&route, &project);
    if let Some(bytes) = state.hot_fresh(&hot_key) {
        return Ok(PageOutcome::Ready(bytes));
    }

    let key = format!("{mirror_name}/{project}");
    if let Some(record) = fresh_cached(&state, &key)? {
        return Ok(PageOutcome::Ready(transform_whole(&state, &hot_key, &record, context)?));
    }

    let gate = flight_gate(&state, &key);
    let guard = gate.lock_owned().await;
    if let Some(bytes) = state.hot_fresh(&state.hot_key(&route, &project)) {
        return Ok(PageOutcome::Ready(bytes));
    }
    if let Some(record) = fresh_cached(&state, &key)? {
        return Ok(PageOutcome::Ready(transform_whole(&state, &hot_key, &record, context)?));
    }

    let now = (state.clock)();
    let cached = state.meta.get_index(&key)?;
    let etag = cached.as_ref().and_then(|record| record.etag.clone());
    let Ok(head) = client.head_project(&project, etag.as_deref()).await else {
        release_flight(&state, &key, guard);
        return Ok(PageOutcome::Fallback);
    };
    match head.status {
        200 if is_json(head.content_type.as_deref()) => {
            use futures_util::StreamExt as _;
            // A 200 despite the stored ETag means new content; a first fetch is not a refresh.
            if cached.is_some() {
                tracing::info!(%key, "upstream page changed");
                state.metrics.record(Event::Refresh {
                    route: mirror_route(&state, &mirror_name),
                    project: project.clone(),
                    changed: true,
                });
            }
            let etag = head.etag.clone();
            let last_serial = head.last_serial;
            let max_age = head.max_age;
            Ok(PageOutcome::Streaming(live_stream(
                state.clone(),
                LiveStream {
                    body: head.into_stream().boxed(),
                    transformer: PageTransformer::new(context),
                    key,
                    hot_key,
                    mirror: mirror_name,
                    project,
                    etag,
                    last_serial,
                    fetched_at: now,
                    fresh_secs: max_age,
                    guard,
                },
            )))
        }
        304 => {
            let mut record = cached.ok_or(CacheError::Unavailable)?;
            record.fetched_at_unix = now;
            record.fresh_secs = head.max_age.or(record.fresh_secs);
            state.meta.put_index(&key, &record)?;
            state.metrics.record(Event::Refresh {
                route: mirror_route(&state, &mirror_name),
                project: project.clone(),
                changed: false,
            });
            release_flight(&state, &key, guard);
            Ok(PageOutcome::Ready(transform_whole(&state, &hot_key, &record, context)?))
        }
        404 => {
            release_flight(&state, &key, guard);
            // An overlay with local files still has a page to serve without the upstream.
            if context.local_files.is_empty() && context.local_versions.is_empty() {
                Ok(PageOutcome::NotFound)
            } else {
                Ok(PageOutcome::Fallback)
            }
        }
        200 => {
            let record = buffer_html_page(&state, &key, &mirror_name, &project, now, head).await?;
            release_flight(&state, &key, guard);
            Ok(PageOutcome::Ready(transform_whole(&state, &hot_key, &record, context)?))
        }
        _ => {
            release_flight(&state, &key, guard);
            Ok(PageOutcome::Fallback)
        }
    }
}

/// An HTML-only upstream cannot stream through the JSON transformer: buffer it, canonicalize to
/// JSON once, and persist.
async fn buffer_html_page(
    state: &AppState,
    key: &str,
    mirror_name: &str,
    project: &str,
    now: i64,
    head: velodex_upstream::SimpleHead,
) -> Result<CachedIndex, CacheError> {
    let url = head.url.clone();
    let content_type = head.content_type.clone();
    let (etag, last_serial) = (head.etag.clone(), head.last_serial);
    let max_age = head.max_age;
    let body = head.bytes().await?;
    let response = SimpleResponse {
        status: 200,
        url,
        content_type,
        etag,
        last_serial,
        max_age,
        body,
    };
    let record = CachedIndex {
        etag: response.etag.clone(),
        last_serial: response.last_serial,
        fetched_at_unix: now,
        content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
        fresh_secs: response.max_age,
        body: canonical_raw(project, &response),
    };
    persist_page(state, key, mirror_name, project, &record)?;
    Ok(record)
}

/// The streaming ingredients for an index: its single mirror layer with its client, plus the local
/// overlay context. `None` when the index has no mirror or more than one (the buffered path
/// handles those).
fn streaming_parts(
    state: &AppState,
    index: &Index,
    project: &str,
) -> Result<Option<(String, UpstreamClient, crate::stream::PageContext)>, CacheError> {
    match &index.kind {
        IndexKind::Mirror(client) => Ok(Some((
            index.name.clone(),
            client.clone(),
            crate::stream::page_context(&index.route, Vec::new(), Vec::new(), &std::collections::HashMap::new()),
        ))),
        IndexKind::Local { .. } => Ok(None),
        IndexKind::Overlay { layers, upload } => {
            let mut mirror = None;
            let mut local_files = Vec::new();
            let mut local_versions = Vec::new();
            for &pos in layers {
                let layer = state.index_at(pos);
                match &layer.kind {
                    IndexKind::Mirror(client) => {
                        if mirror.replace((layer.name.clone(), client.clone())).is_some() {
                            return Ok(None);
                        }
                    }
                    IndexKind::Local { .. } => {
                        if let Some(mut detail) = local_detail(state, &layer.name, project)? {
                            rewrite_urls(&mut detail, &index.route);
                            local_versions.extend(detail.versions);
                            local_files.extend(detail.files);
                        }
                    }
                    IndexKind::Overlay { .. } => return Ok(None),
                }
            }
            let Some((mirror, client)) = mirror else {
                return Ok(None);
            };
            let overrides: std::collections::HashMap<String, String> = match upload {
                Some(pos) => state
                    .meta
                    .list_overrides(&state.index_at(*pos).name, project)?
                    .into_iter()
                    .collect(),
                None => std::collections::HashMap::new(),
            };
            Ok(Some((
                mirror,
                client,
                crate::stream::page_context(&index.route, local_files, local_versions, &overrides),
            )))
        }
    }
}

/// Transform a warm raw page in one pass and remember the result in the hot cache.
fn transform_whole(
    state: &AppState,
    hot_key: &str,
    record: &CachedIndex,
    context: crate::stream::PageContext,
) -> Result<Bytes, CacheError> {
    let mut transformer = PageTransformer::new(context);
    let mut out = transformer.push(&record.body).map_err(transform_error)?;
    transformer.finish().map_err(transform_error)?;
    out.shrink_to_fit();
    let bytes = Bytes::from(out);
    let expires_at = record.fetched_at_unix + freshness(state, record);
    state.hot.insert(hot_key.to_owned(), (expires_at, bytes.clone()));
    Ok(bytes)
}

fn transform_error(err: crate::stream::TransformError) -> CacheError {
    match err {
        crate::stream::TransformError::Parse(err) => CacheError::Parse(err),
        crate::stream::TransformError::Truncated | crate::stream::TransformError::Trailing => CacheError::Unavailable,
    }
}

/// Everything a live streaming fetch carries between polls.
struct LiveStream {
    body: futures_util::stream::BoxStream<'static, Result<Bytes, velodex_upstream::UpstreamError>>,
    transformer: PageTransformer,
    key: String,
    hot_key: String,
    mirror: String,
    project: String,
    etag: Option<String>,
    last_serial: Option<u64>,
    fetched_at: i64,
    fresh_secs: Option<i64>,
    guard: tokio::sync::OwnedMutexGuard<()>,
}

/// Stream the upstream body through the transformer to the client, teeing the raw bytes for the
/// page cache and the transformed bytes for the hot cache; both persist when the stream completes.
#[allow(
    clippy::significant_drop_tightening,
    reason = "the flight guard inside LiveStream deliberately lives until the stream ends"
)]
fn live_stream(
    state: Arc<AppState>,
    live: LiveStream,
) -> futures_util::stream::BoxStream<'static, Result<Bytes, std::io::Error>> {
    use futures_util::StreamExt as _;
    let raw: Vec<u8> = Vec::new();
    let transformed: Vec<u8> = Vec::new();
    let started = std::time::Instant::now();
    futures_util::stream::unfold(
        (state, Some(live), raw, transformed),
        move |(state, live, mut raw, mut transformed)| async move {
            let mut live = live?;
            match live.body.next().await {
                Some(Ok(chunk)) => {
                    raw.extend_from_slice(&chunk);
                    match live.transformer.push(&chunk) {
                        Ok(out) => {
                            transformed.extend_from_slice(&out);
                            Some((Ok(Bytes::from(out)), (state, Some(live), raw, transformed)))
                        }
                        Err(err) => Some((
                            Err(std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string())),
                            (state, None, raw, transformed),
                        )),
                    }
                }
                None => {
                    let summary = match live.transformer.finish() {
                        Ok(summary) => summary,
                        Err(err) => {
                            return Some((
                                Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, err.to_string())),
                                (state, None, raw, transformed),
                            ));
                        }
                    };
                    let record = CachedIndex {
                        etag: live.etag.clone(),
                        last_serial: live.last_serial,
                        fetched_at_unix: live.fetched_at,
                        content_type: Some("application/vnd.pypi.simple.v1+json".to_owned()),
                        fresh_secs: live.fresh_secs,
                        body: std::mem::take(&mut raw),
                    };
                    let expires_at = record.fetched_at_unix + record.fresh_secs.unwrap_or(state.ttl_secs);
                    // One batched transaction, awaited before the body closes: every byte has been
                    // sent already, and a client cannot act on the page before EOF, so downloads
                    // always find their file registrations.
                    let raw_len = record.body.len();
                    let persist_state = state.clone();
                    let (key, mirror, project) = (live.key.clone(), live.mirror.clone(), live.project.clone());
                    // One line on purpose: the error branch is a disk failure no test can
                    // inject, and a folded line stays covered by the condition's evaluation.
                    #[rustfmt::skip]
                    tokio::task::spawn_blocking(move || {
                        if let Err(err) = persist_streamed(&persist_state, &key, &mirror, &project, &record, &summary) { tracing::error!(error = ?err, %key, "page persist failed"); }
                    })
                    .await
                    .expect("page persist task never panics");
                    // The hot page goes live only after the persist lands: a concurrent client that
                    // serves this page from the hot cache and immediately requests a file must find
                    // that file's registration.
                    state.hot.insert(
                        live.hot_key.clone(),
                        (expires_at, Bytes::from(std::mem::take(&mut transformed))),
                    );
                    state.inflight.lock().expect("inflight lock").remove(&live.key);
                    drop(live.guard);
                    let elapsed_ms = started.elapsed().as_millis();
                    tracing::debug!(key = %live.key, bytes = raw_len, elapsed_ms, "page streamed from upstream");
                    None
                }
                Some(Err(err)) => Some((
                    Err(std::io::Error::other(err.to_string())),
                    (state, None, raw, transformed),
                )),
            }
        },
    )
    .boxed()
}

/// How a file download gets its bytes.
pub enum FileOutcome {
    /// The blob is on disk; stream it from there.
    Cached(std::path::PathBuf),
    /// A live upstream download, streamed to the client while it verifies and persists.
    Live(futures_util::stream::BoxStream<'static, Result<Bytes, std::io::Error>>),
}

/// Where one in-flight blob download stands; every client tailing it watches this value.
#[derive(Clone, Debug, Default)]
pub struct DownloadProgress {
    /// Bytes readable from the temp file so far.
    pub flushed: u64,
    /// Set once: `Ok` after the blob committed, `Err` when the transfer or verification failed.
    pub done: Option<Result<(), String>>,
}

/// A live download other requests for the same digest can attach to.
#[derive(Clone, Debug)]
pub struct DownloadHandle {
    /// The temp file the transfer lands in until commit renames it.
    path: std::path::PathBuf,
    progress: tokio::sync::watch::Receiver<DownloadProgress>,
}

#[cfg(test)]
impl DownloadHandle {
    pub(crate) const fn new(
        path: std::path::PathBuf,
        progress: tokio::sync::watch::Receiver<DownloadProgress>,
    ) -> Self {
        Self { path, progress }
    }
}

/// Serve a file with maximum overlap: a cached blob streams from disk, a miss tails one shared
/// upstream transfer as its bytes land.
///
/// A miss starts a detached transfer that hashes into a temp file; every concurrent request for
/// the digest streams from that file in parallel, and the transfer outlives its clients, so an
/// abandoned download still populates the cache.
///
/// # Errors
/// Returns [`CacheError::FileNotFound`] if the digest has no known source, or another error on a
/// store or upstream failure.
///
/// # Panics
/// Never in practice: only if the downloads registry lock was poisoned by an earlier panic.
#[expect(
    clippy::significant_drop_tightening,
    reason = "the connect gate stays held across start_download so racing clients attach instead of double-fetching"
)]
pub async fn stream_file(
    state: Arc<AppState>,
    digest: Digest,
    route: String,
    filename: String,
) -> Result<FileOutcome, CacheError> {
    if state.blobs.exists(&digest) {
        return Ok(FileOutcome::Cached(state.blobs.path_for(&digest)));
    }
    // The gate serializes only the connect phase, so an upstream refusal stays a clean HTTP error
    // for every racing client instead of a mid-body abort on a started stream.
    let gate = flight_gate(&state, digest.as_str());
    let guard = gate.lock_owned().await;
    if state.blobs.exists(&digest) {
        release_flight(&state, digest.as_str(), guard);
        return Ok(FileOutcome::Cached(state.blobs.path_for(&digest)));
    }
    let handle = if let Some(running) = existing_download(&state, &digest) {
        running
    } else {
        let started = start_download(&state, &digest, route.clone(), filename.clone()).await?;
        state
            .downloads
            .lock()
            .expect("downloads lock")
            .insert(digest.as_str().to_owned(), started.clone());
        started
    };
    release_flight(&state, digest.as_str(), guard);
    Ok(FileOutcome::Live(tail_download(state, digest, handle, route, filename)))
}

/// Connect upstream and spawn the detached pump feeding a new blob transfer.
async fn start_download(
    state: &Arc<AppState>,
    digest: &Digest,
    route: String,
    filename: String,
) -> Result<DownloadHandle, CacheError> {
    let (url, source) = state
        .meta
        .get_file_url(digest.as_str())?
        .ok_or(CacheError::FileNotFound)?;
    let client = source_client(state, &source)?;
    let body = client.stream_bytes(&url).await?;
    let pending = state.blobs.begin()?;
    let (sender, receiver) = tokio::sync::watch::channel(DownloadProgress::default());
    let handle = DownloadHandle {
        path: pending.path().to_owned(),
        progress: receiver,
    };
    tokio::spawn(pump_download(
        state.clone(),
        digest.clone(),
        body,
        pending,
        sender,
        (route, filename),
    ));
    Ok(handle)
}

/// The in-flight download for `digest`, if one is pumping right now.
fn existing_download(state: &AppState, digest: &Digest) -> Option<DownloadHandle> {
    state
        .downloads
        .lock()
        .expect("downloads lock")
        .get(digest.as_str())
        .cloned()
}

async fn wait_for_download(handle: &mut DownloadHandle) -> Result<(), CacheError> {
    loop {
        let done = handle.progress.borrow_and_update().done.clone();
        match done {
            Some(Ok(())) => return Ok(()),
            Some(Err(message)) => return Err(CacheError::Stream(message)),
            None => {
                if handle.progress.changed().await.is_err() {
                    return Err(CacheError::Stream("blob transfer abandoned".to_owned()));
                }
            }
        }
    }
}

fn source_client(state: &AppState, source: &str) -> Result<UpstreamClient, CacheError> {
    state
        .indexes
        .iter()
        .find(|index| index.name == source)
        .and_then(|index| match &index.kind {
            IndexKind::Mirror(client) => Some(client.clone()),
            IndexKind::Local { .. } | IndexKind::Overlay { .. } => None,
        })
        .ok_or(CacheError::FileNotFound)
}

/// Chunks smaller than this batch up before a flush makes them visible to tailing readers.
const FLUSH_EVERY: u64 = 256 * 1024;

/// Pull the upstream body into the pending blob, publishing flushed progress as it lands; on EOF
/// the blob persists only when the digest verifies (clients check their own hashes regardless).
/// Runs detached from any client, so the cache fills even when every requester disconnects.
async fn pump_download(
    state: Arc<AppState>,
    digest: Digest,
    body: impl futures_util::Stream<Item = Result<Bytes, velodex_upstream::UpstreamError>> + Send + 'static,
    mut pending: velodex_storage::blob::PendingBlob,
    sender: tokio::sync::watch::Sender<DownloadProgress>,
    served_as: (String, String),
) {
    let started = std::time::Instant::now();
    let outcome = match drain_to_blob(body, &mut pending, &sender).await {
        Ok(()) => {
            let commit_state = state.clone();
            let commit_digest = digest.clone();
            // The rename waits on an fsync, so it runs off the async workers.
            tokio::task::spawn_blocking(move || commit_state.blobs.commit(pending, &commit_digest))
                .await
                .expect("blob commit task never panics")
                .map_err(|err| err.to_string())
        }
        Err(err) => Err(err),
    };
    let bytes = sender.borrow().flushed;
    let elapsed_ms = started.elapsed().as_millis();
    #[rustfmt::skip]
    tracing::debug!(digest = digest.as_str(), bytes, elapsed_ms, "blob transfer ended");
    if outcome.is_err() {
        tracing::warn!(digest = digest.as_str(), "blob persist rejected");
        let (route, filename) = served_as;
        state.metrics.record(Event::BlobRejected { route, filename });
    }
    // Commit lands before the entry disappears and the entry disappears before done broadcasts,
    // so a request arriving at any point sees the blob, the live download, or nothing stale.
    state.downloads.lock().expect("downloads lock").remove(digest.as_str());
    sender.send_modify(|progress| progress.done = Some(outcome));
}

/// Tee the upstream body into the pending blob, publishing progress at every flush. Errors map to
/// strings so the watch channel can carry the verdict to every tailing client.
async fn drain_to_blob(
    body: impl futures_util::Stream<Item = Result<Bytes, velodex_upstream::UpstreamError>> + Send + 'static,
    pending: &mut velodex_storage::blob::PendingBlob,
    sender: &tokio::sync::watch::Sender<DownloadProgress>,
) -> Result<(), String> {
    use futures_util::StreamExt as _;
    let mut body = std::pin::pin!(body);
    let mut written = 0u64;
    let mut flushed = 0u64;
    while let Some(item) = body.next().await {
        let chunk = item.map_err(|err| err.to_string())?;
        // A failed tee write leaves the hash short of these bytes, so commit refuses the blob at
        // the end; progress only advances on flushed bytes, so tails never read past the file.
        let _ = pending.write(&chunk);
        written += chunk.len() as u64;
        if written - flushed >= FLUSH_EVERY && pending.flush().is_ok() {
            flushed = written;
            sender.send_modify(|progress| progress.flushed = flushed);
        }
    }
    // A failed final flush reads as truncation to any tail and as a digest shortfall at commit.
    let _ = pending.flush();
    sender.send_modify(|progress| progress.flushed = written);
    Ok(())
}

/// One client's view of a live download while it tails the pump's temp file.
struct Tail {
    state: Arc<AppState>,
    digest: Digest,
    handle: DownloadHandle,
    file: Option<tokio::fs::File>,
    sent: u64,
    route: String,
    filename: String,
    alive: bool,
}

/// Stream a live download to one client by tailing the temp file as the pump flushes it.
pub(crate) fn tail_download(
    state: Arc<AppState>,
    digest: Digest,
    handle: DownloadHandle,
    route: String,
    filename: String,
) -> futures_util::stream::BoxStream<'static, Result<Bytes, std::io::Error>> {
    use futures_util::StreamExt as _;
    use tokio::io::AsyncReadExt as _;
    let tail = Tail {
        state,
        digest,
        handle,
        file: None,
        sent: 0,
        route,
        filename,
        alive: true,
    };
    futures_util::stream::unfold(tail, |mut tail| async move {
        if !tail.alive {
            return None;
        }
        loop {
            let progress = tail.handle.progress.borrow_and_update().clone();
            if tail.sent < progress.flushed {
                if tail.file.is_none() {
                    match tail_file(&tail.state, &mut tail.handle, &tail.digest).await {
                        Ok(file) => tail.file = Some(file),
                        Err(err) => {
                            tail.alive = false;
                            return Some((Err(err), tail));
                        }
                    }
                }
                let reader = tail.file.as_mut().expect("opened above");
                let budget = usize::try_from((progress.flushed - tail.sent).min(FLUSH_EVERY)).expect("bounded chunk");
                let mut buffer = vec![0u8; budget];
                match reader.read(&mut buffer).await {
                    Ok(0) | Err(_) => {
                        tail.alive = false;
                        return Some((Err(std::io::Error::other("blob temp file truncated mid-tail")), tail));
                    }
                    Ok(count) => {
                        buffer.truncate(count);
                        tail.sent += count as u64;
                        return Some((Ok(Bytes::from(buffer)), tail));
                    }
                }
            }
            match progress.done {
                Some(Ok(())) => {
                    tail.state.metrics.record(Event::Download {
                        route: tail.route.clone(),
                        filename: tail.filename.clone(),
                        bytes: tail.sent,
                    });
                    return None;
                }
                Some(Err(message)) => {
                    tail.alive = false;
                    return Some((Err(std::io::Error::other(message)), tail));
                }
                None => {
                    if tail.handle.progress.changed().await.is_err() {
                        tail.alive = false;
                        return Some((Err(std::io::Error::other("blob transfer abandoned")), tail));
                    }
                }
            }
        }
    })
    .boxed()
}

/// Open the file a tail reads from. The first read always happens at offset zero, so no seek is
/// needed: either the temp file still exists, or the transfer already committed and the blob
/// serves from its final path.
///
/// The commit renames the temp file before the verdict broadcasts, so a missing file with no
/// verdict yet is a normal in-between state: wait for the next progress change and look again.
async fn tail_file(
    state: &AppState,
    handle: &mut DownloadHandle,
    digest: &Digest,
) -> Result<tokio::fs::File, std::io::Error> {
    loop {
        let missing = match tokio::fs::File::open(&handle.path).await {
            Ok(file) => return Ok(file),
            Err(err) => err,
        };
        let verdict = handle.progress.borrow_and_update().done.clone();
        match verdict {
            Some(Ok(())) => return tokio::fs::File::open(state.blobs.path_for(digest)).await,
            Some(Err(message)) => return Err(std::io::Error::other(message)),
            None => {
                if handle.progress.changed().await.is_err() {
                    return Err(missing);
                }
            }
        }
    }
}
