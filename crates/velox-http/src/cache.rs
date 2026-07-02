//! The read-through cache and index composition: serve a project's simple page and file bytes across
//! an index's layers, fetching and caching from upstream on a miss.

use std::collections::{BTreeSet, HashSet};

use bytes::Bytes;
use url::Url;
use velox_core::pypi::file_matches_version;
use velox_core::pypi::{
    CoreMetadata, File, Meta, ParsedDetail, ProjectDetail, ProjectList, ProjectListEntry, Yanked, parse_detail,
    parse_detail_html, to_json,
};
use velox_storage::blob::Digest;
use velox_storage::meta::CachedIndex;
use velox_upstream::{SimpleResponse, UpstreamClient};

use crate::state::{AppState, Index, IndexKind};
use crate::upload::{PreparedUpload, Uploaded};

/// An error while producing a cached response.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error(transparent)]
    Meta(#[from] velox_storage::meta::MetaError),
    #[error(transparent)]
    Blob(#[from] velox_storage::blob::BlobError),
    #[error(transparent)]
    Upstream(#[from] velox_upstream::UpstreamError),
    #[error(transparent)]
    Parse(#[from] serde_json::Error),
    #[error("upstream unreachable and nothing cached")]
    Unavailable,
    #[error("index is not volatile; delete is disabled")]
    NotVolatile,
    #[error("no known source for this file")]
    FileNotFound,
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
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    let mut versions = BTreeSet::new();
    let mut found = false;
    for &pos in layers {
        let layer = state.index_at(pos);
        // A layer being unavailable (a down mirror with a cold cache) must not break the others.
        let resolved = match Box::pin(resolve_detail(state, layer, project, serve_route)).await {
            Ok(resolved) => resolved,
            Err(err) => {
                tracing::warn!(layer = %layer.name, error = ?err, "overlay layer unavailable, skipping");
                continue;
            }
        };
        if let Some(detail) = resolved {
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
async fn mirror_detail(
    state: &AppState,
    name: &str,
    route: &str,
    client: &UpstreamClient,
    project: &str,
) -> Result<Option<ProjectDetail>, CacheError> {
    let key = format!("{name}/{project}");
    let now = (state.clock)();
    let cached = state.meta.get_index(&key)?;

    if let Some(record) = &cached
        && now - record.fetched_at_unix < state.ttl_secs
    {
        return Ok(Some(decode_detail(&record.body)?));
    }

    let etag = cached.as_ref().and_then(|record| record.etag.clone());
    match client.fetch_project(project, etag.as_deref()).await {
        Ok(response) if response.status == 200 => {
            Ok(Some(store_fresh(state, &key, name, route, project, response, now)?))
        }
        Ok(response) if response.status == 304 => {
            let mut record = cached.ok_or(CacheError::Unavailable)?;
            record.fetched_at_unix = now;
            state.meta.put_index(&key, &record)?;
            Ok(Some(decode_detail(&record.body)?))
        }
        Ok(response) if response.status == 404 => Ok(None),
        Ok(_) => serve_stale(cached.as_ref()),
        Err(err) => match cached.as_ref() {
            Some(record) => Ok(Some(decode_detail(&record.body)?)),
            None => Err(CacheError::Upstream(err)),
        },
    }
}

fn serve_stale(cached: Option<&CachedIndex>) -> Result<Option<ProjectDetail>, CacheError> {
    match cached {
        Some(record) => Ok(Some(decode_detail(&record.body)?)),
        None => Err(CacheError::Unavailable),
    }
}

fn store_fresh(
    state: &AppState,
    key: &str,
    name: &str,
    route: &str,
    project: &str,
    response: SimpleResponse,
    now: i64,
) -> Result<ProjectDetail, CacheError> {
    let parsed = parse_upstream(project, response.content_type.as_deref(), &response.url, &response.body)?;
    let files = parsed
        .files
        .into_iter()
        .map(|file| register_file(state, file, name, route))
        .collect::<Result<Vec<_>, _>>()?;
    let detail = ProjectDetail {
        meta: Meta::default(),
        name: parsed.name,
        versions: parsed.versions,
        files,
    };
    let record = CachedIndex {
        etag: response.etag,
        last_serial: response.last_serial,
        fetched_at_unix: now,
        body: to_json(&detail).into_bytes(),
    };
    state.meta.put_index(key, &record)?;
    state.meta.put_project(name, project, &detail.name)?;
    Ok(detail)
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

/// Point every content-addressable file at velox's own file route on `route`.
fn rewrite_urls(detail: &mut ProjectDetail, route: &str) {
    for file in &mut detail.files {
        if let Some(sha256) = file.hashes.get("sha256") {
            file.url = format!("/{route}/files/{sha256}/{}", file.filename);
        }
    }
}

/// The project names velox has observed on `index`, unioned across an overlay's layers.
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

/// Parse an upstream simple page as PEP 691 JSON, or fall back to PEP 503 HTML for indexes that do
/// not serve JSON.
fn parse_upstream(
    project: &str,
    content_type: Option<&str>,
    url: &Url,
    body: &[u8],
) -> Result<ParsedDetail, CacheError> {
    if content_type.is_some_and(|content_type| content_type.contains("json")) {
        Ok(parse_detail(body)?)
    } else {
        Ok(parse_detail_html(project, &String::from_utf8_lossy(body), url))
    }
}

/// Record a mirror file's upstream URL and PEP 658 sibling under `source` (the mirror name, for
/// auth on fetch), then set its URL to velox's own file route on `route`.
fn register_file(state: &AppState, mut file: File, source: &str, route: &str) -> Result<File, CacheError> {
    let Some(sha256) = file.hashes.get("sha256").cloned() else {
        file.core_metadata = CoreMetadata::Absent;
        return Ok(file);
    };
    state.meta.put_file_url(&sha256, &file.url, source)?;
    match metadata_sha256(&file.core_metadata) {
        Some(metadata_digest) => {
            state
                .meta
                .put_metadata(&sha256, &format!("{}.metadata", file.url), &metadata_digest, source)?;
        }
        None => file.core_metadata = CoreMetadata::Absent,
    }
    file.url = format!("/{route}/files/{sha256}/{}", file.filename);
    Ok(file)
}

fn metadata_sha256(core_metadata: &CoreMetadata) -> Option<String> {
    match core_metadata {
        CoreMetadata::Hashes(hashes) => hashes.get("sha256").cloned(),
        CoreMetadata::Absent | CoreMetadata::Available => None,
    }
}

/// Fetch a URL through the named mirror's client (reusing its authentication).
async fn fetch_from_source(state: &AppState, source: &str, url: &str) -> Result<Bytes, CacheError> {
    let client = state
        .indexes
        .iter()
        .find(|index| index.name == source)
        .and_then(|index| match &index.kind {
            IndexKind::Mirror(client) => Some(client),
            IndexKind::Local { .. } | IndexKind::Overlay { .. } => None,
        })
        .ok_or(CacheError::FileNotFound)?;
    Ok(client.fetch_bytes(url).await?)
}

/// Resolve a file's bytes: serve the cached blob, or fetch it from its source mirror, verify, cache.
///
/// # Errors
/// Returns [`CacheError::FileNotFound`] if the digest has no known source, or another error on a
/// store or upstream failure.
pub async fn file_bytes(state: &AppState, digest: &Digest) -> Result<Bytes, CacheError> {
    if state.blobs.exists(digest) {
        return Ok(Bytes::from(state.blobs.read(digest)?));
    }
    let (url, source) = state
        .meta
        .get_file_url(digest.as_str())?
        .ok_or(CacheError::FileNotFound)?;
    let bytes = fetch_from_source(state, &source, &url).await?;
    state.blobs.write_verified(&bytes, digest)?;
    Ok(bytes)
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

/// Persist a prepared upload into the local store `name`: write the blob, record the file and its
/// project, and bump the serial.
///
/// # Errors
/// Returns [`CacheError`] if a blob write, store write, or encode fails.
pub fn store_upload(state: &AppState, name: &str, prepared: &PreparedUpload) -> Result<(), CacheError> {
    state.blobs.write_verified(&prepared.content, &prepared.digest)?;
    let mut record = prepared.record.clone();
    // A wheel's own METADATA becomes its PEP 658 sibling, as pypi.org serves for uploads. The
    // sibling blob is stored outright, so `metadata_bytes` never needs an upstream URL for it.
    if let Some(metadata) = crate::archive::wheel_metadata(&prepared.filename, &prepared.content) {
        let digest = state.blobs.write(&metadata)?;
        state
            .meta
            .put_metadata(prepared.digest.as_str(), "uploaded", digest.as_str(), name)?;
        record.file.core_metadata = CoreMetadata::Hashes(std::collections::BTreeMap::from([(
            "sha256".to_owned(),
            digest.as_str().to_owned(),
        )]));
    }
    let record = to_json(&record).into_bytes();
    state
        .meta
        .put_upload(name, &prepared.normalized, &prepared.filename, &record)?;
    state
        .meta
        .put_project(name, &prepared.normalized, &prepared.display_name)?;
    state.meta.next_serial()?;
    Ok(())
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

fn decode_detail(body: &[u8]) -> Result<ProjectDetail, CacheError> {
    let parsed = parse_detail(body)?;
    Ok(ProjectDetail {
        meta: Meta::default(),
        name: parsed.name,
        versions: parsed.versions,
        files: parsed.files,
    })
}
