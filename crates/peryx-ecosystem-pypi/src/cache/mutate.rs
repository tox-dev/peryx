//! Hosted-store mutations: uploads, promotion, yank/hide overrides, and project status.

use std::collections::HashSet;

use crate::store::PypiStore as _;
use crate::store::{Guard, UploadMutation};
use crate::upload::{self, PreparedUpload, TrashInfo, Uploaded};
use crate::{ProjectStatus, Yanked, file_matches_version, parse_distribution_filename, to_json, versions_match};
use peryx_core::path::local_file_url;
use peryx_driver::state::ServingState;
use peryx_index::{Index, IndexKind};

use super::CacheError;
use super::resolve::resolve_detail;

/// Persist a prepared upload into the hosted store `name`: commit the staged blob, record the file
/// and its project, and bump the serial. Returns `false` for a same-bytes duplicate.
///
/// # Errors
/// Returns [`CacheError`] if a blob write, store write, or encode fails.
pub fn store_upload(state: &ServingState, name: &str, prepared: PreparedUpload) -> Result<bool, CacheError> {
    let project = prepared.normalized.clone();
    let stored = upload::store_prepared(&state.meta, &state.blobs, name, prepared)?;
    if stored {
        state.invalidate_project(&project);
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
    state: &ServingState,
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
        if !versions_match(&uploaded.version, version) {
            continue;
        }
        matched = true;
        let digest = uploaded
            .file
            .hashes
            .get("sha256")
            .cloned()
            .ok_or_else(|| CacheError::MissingSha256(filename.clone()))?;
        uploaded.file.url = local_file_url(target_route, &digest, &filename);
        records.push((filename, digest, to_json(&uploaded).into_bytes()));
    }
    if !matched {
        return Err(CacheError::NoPromotableFiles {
            source_index: source.to_owned(),
            project: normalized.to_owned(),
            version: version.to_owned(),
        });
    }
    let display = state
        .meta
        .get_project(source, normalized)?
        .unwrap_or_else(|| normalized.to_owned());
    let promoted = state
        .meta
        .promote_files_checked(target, normalized, &display, &records, promote_conflict)?;
    if promoted > 0 {
        state.invalidate_project(normalized);
    }
    Ok(promoted)
}

/// The promotion precondition for one target filename, evaluated inside the write transaction: a
/// free target is copied, an identical one left as it is, and a target holding different bytes is a
/// conflict — so a concurrent upload to the target cannot be silently overwritten.
fn promote_conflict(filename: &str, digest: &str, existing: Option<&[u8]>) -> Result<Guard, CacheError> {
    let Some(existing) = existing else {
        return Ok(Guard::Commit);
    };
    let existing: Uploaded = serde_json::from_slice(existing)?;
    if existing.file.hashes.get("sha256").is_some_and(|hash| hash == digest) {
        Ok(Guard::Skip)
    } else {
        Err(CacheError::FileExists(filename.to_owned()))
    }
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
    state: &ServingState,
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
        state.invalidate_project(normalized);
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

/// The provenance a soft-delete records on each file it trashes, threaded from the delete request.
#[derive(Clone, Copy)]
pub struct TrashContext<'a> {
    pub deleted_at_unix: i64,
    pub actor: Option<&'a str>,
    pub reason: Option<&'a str>,
}

/// Remove a project's files as served by `index`.
///
/// Uploaded files are soft-deleted (requires `volatile`): the record is marked trashed and its blob
/// kept, so the file drops out of every served page but stays recoverable until a restore or a purge.
/// Read-only upstream files get a reversible `hidden` override on `hosted`. Returns how many files
/// were affected.
///
/// # Errors
/// Returns [`CacheError::NotVolatile`] when uploaded files match but the hosted store is not
/// volatile, or another [`CacheError`] on a store or resolution failure.
pub async fn remove_files(
    state: &ServingState,
    index: &Index,
    hosted: &str,
    volatile: bool,
    normalized: &str,
    version: Option<&str>,
    trash: TrashContext<'_>,
) -> Result<usize, CacheError> {
    let filenames = served_filenames(state, index, normalized, version).await?;
    let uploaded = upload_filenames(state, hosted, normalized)?;
    let mut affected = trash_uploads(state, hosted, volatile, normalized, version, trash)?;
    for filename in filenames {
        if uploaded.contains(&filename) {
            continue;
        }
        state.meta.put_override(hosted, normalized, &filename, HIDDEN)?;
        affected += 1;
    }
    if affected > 0 {
        state.invalidate_project(normalized);
    }
    Ok(affected)
}

/// Restore a project's files (optionally one version): clear `hidden` overrides so a deleted upstream
/// file reappears, and un-trash soft-deleted uploaded files. Returns how many files reappeared.
///
/// # Errors
/// Returns [`CacheError`] on a store failure.
pub fn restore_files(
    state: &ServingState,
    hosted: &str,
    normalized: &str,
    version: Option<&str>,
) -> Result<usize, CacheError> {
    let mut restored = untrash_uploads(state, hosted, normalized, version)?;
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
        state.invalidate_project(normalized);
    }
    Ok(restored)
}

/// Resolve the effective project status for upload policy checks. A missing project is active.
///
/// # Errors
/// Returns [`CacheError`] on a store, parse, or upstream failure.
pub async fn project_status(
    state: &ServingState,
    index: &Index,
    normalized: &str,
) -> Result<ProjectStatus, CacheError> {
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
pub fn download_status(state: &ServingState, index: &Index, filename: &str) -> Result<ProjectStatus, CacheError> {
    let artifact = filename.strip_suffix(".metadata").unwrap_or(filename);
    let Ok(parsed) = parse_distribution_filename(artifact) else {
        return Ok(ProjectStatus::Active);
    };
    stored_project_status(state, index, &parsed.normalized_name)
}

fn stored_project_status(state: &ServingState, index: &Index, normalized: &str) -> Result<ProjectStatus, CacheError> {
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

fn status_for_index(state: &ServingState, index: &str, normalized: &str) -> Result<ProjectStatus, CacheError> {
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
    state: &ServingState,
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

fn upload_filenames(state: &ServingState, hosted: &str, normalized: &str) -> Result<HashSet<String>, CacheError> {
    Ok(state
        .meta
        .list_upload_entries(hosted, normalized)?
        .into_iter()
        .map(|(filename, _)| filename)
        .collect())
}

/// Mark uploaded records trashed, optionally limited to one version. An already-trashed record is
/// left as it is (delete is idempotent), and a non-volatile store rejects a live match rather than
/// touching it. The blob is never removed here, so the file stays recoverable. Returns how many
/// records were trashed.
fn trash_uploads(
    state: &ServingState,
    name: &str,
    volatile: bool,
    normalized: &str,
    version: Option<&str>,
    trash: TrashContext<'_>,
) -> Result<usize, CacheError> {
    state
        .meta
        .mutate_uploads(name, normalized, "delete-file", |_filename, bytes| {
            let mut uploaded: Uploaded = serde_json::from_slice(bytes)?;
            if version.is_some_and(|version| !versions_match(&uploaded.version, version)) || uploaded.trashed.is_some()
            {
                return Ok(UploadMutation::Keep);
            }
            if !volatile {
                return Err(CacheError::NotVolatile);
            }
            uploaded.trashed = Some(TrashInfo {
                deleted_at_unix: trash.deleted_at_unix,
                actor: trash.actor.map(str::to_owned),
                reason: trash.reason.map(str::to_owned),
            });
            Ok(UploadMutation::Replace(to_json(&uploaded).into_bytes()))
        })
}

/// Clear the trashed marker off soft-deleted uploaded records, optionally limited to one version, so
/// the files return to every served page. Returns how many records were restored.
fn untrash_uploads(
    state: &ServingState,
    name: &str,
    normalized: &str,
    version: Option<&str>,
) -> Result<usize, CacheError> {
    state
        .meta
        .mutate_uploads(name, normalized, "restore", |_filename, bytes| {
            let mut uploaded: Uploaded = serde_json::from_slice(bytes)?;
            if uploaded.trashed.is_none() || version.is_some_and(|version| !versions_match(&uploaded.version, version))
            {
                return Ok(UploadMutation::Keep);
            }
            uploaded.trashed = None;
            Ok(UploadMutation::Replace(to_json(&uploaded).into_bytes()))
        })
}

/// Set the yank state of uploaded files, optionally limited to one version. Returns how many
/// changed.
fn yank_uploads(
    state: &ServingState,
    name: &str,
    normalized: &str,
    version: Option<&str>,
    yanked: &Yanked,
) -> Result<usize, CacheError> {
    let action = if matches!(yanked, Yanked::No) { "unyank" } else { "yank" };
    state.meta.mutate_uploads(name, normalized, action, |_filename, bytes| {
        let mut uploaded: Uploaded = serde_json::from_slice(bytes)?;
        if version.is_some_and(|version| !versions_match(&uploaded.version, version)) || uploaded.file.yanked == *yanked
        {
            return Ok(UploadMutation::Keep);
        }
        uploaded.file.yanked = yanked.clone();
        Ok(UploadMutation::Replace(to_json(&uploaded).into_bytes()))
    })
}
