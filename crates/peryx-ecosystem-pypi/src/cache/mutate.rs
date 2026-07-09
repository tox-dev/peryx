//! Hosted-store mutations: uploads, promotion, yank/hide overrides, and project status.

use std::collections::HashSet;

use crate::file_matches_version;
use crate::upload::{self, PreparedUpload, Uploaded};
use crate::{ProjectStatus, Yanked, parse_distribution_filename, to_json};
use peryx_http::path_safety::local_file_url;
use peryx_http::state::{AppState, Index, IndexKind};

use super::CacheError;
use super::resolve::resolve_detail;

/// Persist a prepared upload into the hosted store `name`: commit the staged blob, record the file
/// and its project, and bump the serial. Returns `false` for a same-bytes duplicate.
///
/// # Errors
/// Returns [`CacheError`] if a blob write, store write, or encode fails.
pub fn store_upload(state: &AppState, name: &str, prepared: PreparedUpload) -> Result<bool, CacheError> {
    let stored = upload::store_prepared(&state.meta, &state.blobs, name, prepared)?;
    if stored {
        state.bump_epoch();
    }
    Ok(stored)
}

/// Copy one uploaded release from one hosted layer to another without touching blob bytes.
///
/// # Errors
/// Returns [`CacheError::NoPromotableFiles`] when the source hosted layer has no matching upload,
/// [`CacheError::FileExists`] when a target filename exists with different bytes, or another
/// [`CacheError`] on metadata-store or decode failures.
pub fn promote_release(
    state: &AppState,
    source: &str,
    target: &str,
    target_route: &str,
    normalized: &str,
    version: &str,
) -> Result<usize, CacheError> {
    let mut matched = false;
    let mut records = Vec::new();
    for (filename, bytes) in state.meta.list_upload_entries(source, normalized)? {
        let mut uploaded: Uploaded = serde_json::from_slice(&bytes)?;
        if uploaded.version != version {
            continue;
        }
        matched = true;
        let digest = uploaded
            .file
            .hashes
            .get("sha256")
            .cloned()
            .ok_or_else(|| CacheError::MissingSha256(filename.clone()))?;
        if let Some(existing) = state.meta.get_upload(target, normalized, &filename)? {
            let existing: Uploaded = serde_json::from_slice(&existing)?;
            if existing.file.hashes.get("sha256").is_some_and(|hash| hash == &digest) {
                continue;
            }
            return Err(CacheError::FileExists(filename));
        }
        uploaded.file.url = local_file_url(target_route, &digest, &filename);
        records.push((filename, to_json(&uploaded).into_bytes()));
    }
    if !matched {
        return Err(CacheError::NoPromotableFiles {
            source_index: source.to_owned(),
            project: normalized.to_owned(),
            version: version.to_owned(),
        });
    }
    if records.is_empty() {
        return Ok(0);
    }
    let display = state
        .meta
        .get_project(source, normalized)?
        .unwrap_or_else(|| normalized.to_owned());
    state.meta.put_uploads(target, normalized, &display, &records)?;
    state.meta.append_journal("promote", normalized, None, None)?;
    state.bump_epoch();
    Ok(records.len())
}

/// The two reversible override kinds for files served from read-only layers.
const YANKED: &str = "yanked";

const HIDDEN: &str = "hidden";

/// Set or clear the yank state of a project's files as served by `index`.
///
/// Uploaded files get their stored record rewritten; read-only upstream files get a `yanked`
/// override on `hosted`. Returns how many files changed.
///
/// # Errors
/// Returns [`CacheError`] on a store, decode, or resolution failure.
pub async fn set_yanked(
    state: &AppState,
    index: &Index,
    hosted: &str,
    normalized: &str,
    version: Option<&str>,
    yanked: Yanked,
) -> Result<usize, CacheError> {
    let uploaded = upload_filenames(state, hosted, normalized)?;
    let mut changed = yank_uploads(state, hosted, normalized, version, &yanked)?;
    for filename in served_filenames(state, index, normalized, version).await? {
        if uploaded.contains(&filename) {
            continue;
        }
        if let Some(value) = yank_override_value(&yanked)? {
            state.meta.put_override(hosted, normalized, &filename, &value)?;
            changed += 1;
        } else if state.meta.delete_override(hosted, normalized, &filename)? {
            changed += 1;
        }
    }
    if changed > 0 {
        state.bump_epoch();
    }
    Ok(changed)
}

fn yank_override_value(yanked: &Yanked) -> Result<Option<String>, CacheError> {
    Ok(match yanked {
        Yanked::No => None,
        Yanked::Yes => Some(YANKED.to_owned()),
        Yanked::Reason(reason) => Some(serde_json::to_string(&serde_json::json!({
            "kind": YANKED,
            "reason": reason,
        }))?),
    })
}

/// Remove a project's files as served by `index`.
///
/// Uploaded files are deleted outright (requires `volatile`); read-only upstream files get a
/// reversible `hidden` override on `hosted`. Returns how many files were affected.
///
/// # Errors
/// Returns [`CacheError::NotVolatile`] when uploaded files match but the hosted store is not
/// volatile, or another [`CacheError`] on a store or resolution failure.
pub async fn remove_files(
    state: &AppState,
    index: &Index,
    hosted: &str,
    volatile: bool,
    normalized: &str,
    version: Option<&str>,
) -> Result<usize, CacheError> {
    let uploaded = upload_filenames(state, hosted, normalized)?;
    let mut affected = 0;
    let mut matched_upload = false;
    for filename in served_filenames(state, index, normalized, version).await? {
        if uploaded.contains(&filename) {
            matched_upload = true;
            if !volatile {
                return Err(CacheError::NotVolatile);
            }
            if state.meta.delete_upload(hosted, normalized, &filename)? {
                affected += 1;
            }
        } else {
            state.meta.put_override(hosted, normalized, &filename, HIDDEN)?;
            affected += 1;
        }
    }
    // A versioned delete whose filenames carry no parsable version misses the served-page filter;
    // fall back to matching the version stored in the upload records. A project-level delete never
    // needs this: every upload is on the served page.
    if !matched_upload && let Some(version) = version {
        affected += delete_uploads_of_version(state, hosted, normalized, version)?;
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
    hosted: &str,
    normalized: &str,
    version: Option<&str>,
) -> Result<usize, CacheError> {
    let mut restored = 0;
    for (filename, kind) in state.meta.list_overrides(hosted, normalized)? {
        if kind != HIDDEN {
            continue;
        }
        if version.is_some_and(|version| !file_matches_version(&filename, version)) {
            continue;
        }
        if state.meta.delete_override(hosted, normalized, &filename)? {
            restored += 1;
        }
    }
    if restored > 0 {
        state.bump_epoch();
    }
    Ok(restored)
}

/// Resolve the effective project status for upload policy checks. A missing project is active.
///
/// # Errors
/// Returns [`CacheError`] on a store, parse, or upstream failure.
pub async fn project_status(state: &AppState, index: &Index, normalized: &str) -> Result<ProjectStatus, CacheError> {
    if matches!(index.kind, IndexKind::Hosted { .. }) {
        return Ok(ProjectStatus::Active);
    }
    let Some(detail) = Box::pin(resolve_detail(state, index, normalized, &index.route)).await? else {
        return Ok(ProjectStatus::Active);
    };
    Ok(detail.meta.status())
}

/// Check stored status metadata before serving a content-addressed file download.
///
/// # Errors
/// Returns [`CacheError`] when the store cannot be read.
pub fn download_status(state: &AppState, index: &Index, filename: &str) -> Result<ProjectStatus, CacheError> {
    let artifact = filename.strip_suffix(".metadata").unwrap_or(filename);
    let Ok(parsed) = parse_distribution_filename(artifact) else {
        return Ok(ProjectStatus::Active);
    };
    stored_project_status(state, index, &parsed.normalized_name)
}

fn stored_project_status(state: &AppState, index: &Index, normalized: &str) -> Result<ProjectStatus, CacheError> {
    match &index.kind {
        IndexKind::Cached { .. } => status_for_index(state, &index.name, normalized),
        IndexKind::Hosted { .. } => Ok(ProjectStatus::Active),
        IndexKind::Virtual { layers, .. } => {
            for &pos in layers {
                let status = stored_project_status(state, state.index_at(pos), normalized)?;
                if status != ProjectStatus::Active {
                    return Ok(status);
                }
            }
            Ok(ProjectStatus::Active)
        }
    }
}

fn status_for_index(state: &AppState, index: &str, normalized: &str) -> Result<ProjectStatus, CacheError> {
    Ok(state
        .meta
        .get_project_status(index, normalized)?
        .and_then(|record| record.status)
        .as_deref()
        .and_then(ProjectStatus::from_marker)
        .unwrap_or(ProjectStatus::Active))
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

fn upload_filenames(state: &AppState, hosted: &str, normalized: &str) -> Result<HashSet<String>, CacheError> {
    Ok(state
        .meta
        .list_upload_entries(hosted, normalized)?
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
