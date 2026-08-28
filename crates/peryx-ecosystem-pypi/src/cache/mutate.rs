use std::collections::{BTreeMap, HashSet};

use crate::quota::PendingQuota;
use crate::store::PypiStore as _;
use crate::store::{Guard, PromotedRelease, UploadMutation};
use crate::upload::{self, PreparedUpload, TrashInfo, Uploaded};
use crate::{ProjectStatus, Yanked, file_matches_version, parse_distribution_filename, to_json, versions_match};
use peryx_core::path::local_artifact_url;
use peryx_driver::state::ServingState;
use peryx_index::{Index, IndexKind};

use super::CacheError;
use super::resolve::resolve_detail_optional;

/// A metadata control routes through the project's ownership authority: the normalized project name is
/// the authority key, so every PEP 503 name variant fences on one epoch. The control snapshots that
/// epoch before it reads, then re-admits it before it writes, so a home transfer that lands while the
/// control resolves is caught before it can change serial or visibility under a superseded epoch.
///
/// Snapshot the committed epoch a control will finalize under. `0` when this process runs no ownership
/// group, or the authority has no committed home, which admits the control unfenced.
async fn control_epoch(state: &ServingState, authority: &str) -> u64 {
    state.committed_authority_epoch(authority).await
}

/// Fence a control whose leased epoch a transfer superseded, before it writes. A former home that read
/// the project at `fence` but whose authority advanced is rejected, so its stale write never lands. A
/// process with no group, or an unhomed authority (`fence` of `0`), holds no epoch and is never fenced.
async fn admit_control(state: &ServingState, authority: &str, fence: u64) -> Result<(), CacheError> {
    if fence != 0 && !state.admit_authority_epoch(authority, fence).await {
        return Err(CacheError::AuthoritySuperseded);
    }
    Ok(())
}

/// Persist a prepared upload into the hosted store `name`: commit the staged blobs, record the file
/// and its project, and bump the serial. Returns `false` for a same-bytes duplicate.
///
/// The publish fences on the project's ownership authority like every other mutation: it snapshots the
/// committed epoch, commits the blobs, then re-admits the epoch before the record write. A publish that
/// reads the project as its home but whose authority moved home mid-store is rejected before the record
/// lands, so a stale home never assigns a serial or makes a file visible under a superseded epoch.
///
/// # Errors
/// Returns [`CacheError::AuthoritySuperseded`] when a home transfer superseded the epoch mid-store, or
/// another [`CacheError`] if a blob write, store write, or encode fails.
pub struct StoredUpload {
    pub stored: bool,
    pub commit: Option<peryx_storage::meta::JournalCommit>,
}

/// Stage and publish an upload under the exact authority epoch its caller resolved.
///
/// # Errors
/// Returns [`CacheError::AuthoritySuperseded`] when the epoch moved, or a store error when staging or
/// publication fails.
pub async fn store_upload(
    state: &ServingState,
    name: &str,
    project: &str,
    prepared: PreparedUpload,
    quota: Option<PendingQuota>,
    fence: u64,
) -> Result<StoredUpload, CacheError> {
    let publish = upload::stage_publish(&state.blobs, prepared).await?;
    admit_control(state, project, fence).await?;
    let published = upload::commit_publish(&state.meta, name, publish, quota, crate::replication_enabled(state))?;
    for (digest, size) in &published.placements {
        state.record_home_placement(digest.as_str(), *size, fence);
    }
    state.record_operation_trace(peryx_driver::state::OperationKind::Publish, fence);
    if published.stored {
        state.invalidate_resource(project);
    }
    Ok(StoredUpload {
        stored: published.stored,
        commit: published.commit,
    })
}

/// Copy one uploaded release from one hosted layer to another without touching blob bytes.
///
/// # Errors
/// Returns [`CacheError::NoPromotableFiles`] when the source hosted layer has no matching upload,
/// [`CacheError::FileExists`] when a target filename exists with different bytes, or another
/// [`CacheError`] on metadata-store or decode failures.
pub async fn promote_release(
    state: &ServingState,
    source: &str,
    target: &str,
    target_route: &str,
    normalized: &str,
    version: &str,
) -> Result<usize, CacheError> {
    let fence = control_epoch(state, normalized).await;
    let mut matched = false;
    let mut records = Vec::new();
    let mut blob_sizes = BTreeMap::new();
    for (filename, bytes) in state.meta.list_upload_entries(source, normalized)? {
        let mut uploaded: Uploaded = serde_json::from_slice(&bytes)?;
        if uploaded.trashed.is_some() || !versions_match(&uploaded.version, version) {
            continue;
        }
        matched = true;
        let digest = uploaded
            .file
            .hashes
            .get("sha256")
            .cloned()
            .ok_or_else(|| CacheError::MissingSha256(filename.clone()))?;
        if let Some(size) = uploaded.file.size {
            blob_sizes.insert(digest.clone(), size);
        }
        uploaded.file.url = local_artifact_url(target_route, &digest, &filename);
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
    admit_control(state, normalized, fence).await?;
    let release = PromotedRelease {
        index: target,
        normalized,
        display: &display,
        records: &records,
        blob_sizes: &blob_sizes,
        submitted_at_unix: (state.clock)(),
    };
    let promoted = state
        .meta
        .promote_files_checked(crate::replication_enabled(state), &release, promote_conflict)?;
    if promoted > 0 {
        state.invalidate_resource(normalized);
    }
    Ok(promoted)
}

/// The promotion precondition for one target filename, evaluated inside the write transaction: a
/// free target is copied, an identical one left as it is, and a target holding different bytes is a
/// conflict - so a concurrent upload to the target cannot be silently overwritten.
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
    let fence = control_epoch(state, normalized).await;
    let uploaded = upload_filenames(state, hosted, normalized)?;
    let served = served_filenames(state, index, normalized, version).await?;
    admit_control(state, normalized, fence).await?;
    let submitted_at_unix = (state.clock)();
    let mut changed = yank_uploads(state, hosted, normalized, version, &yanked, submitted_at_unix)?;
    for filename in served {
        if uploaded.contains(&filename) {
            continue;
        }
        if let Some(value) = yank_override_value(&yanked)? {
            state.meta.put_override(
                crate::replication_enabled(state),
                hosted,
                normalized,
                &filename,
                &value,
                submitted_at_unix,
            )?;
            changed += 1;
        } else if state.meta.delete_override(
            crate::replication_enabled(state),
            hosted,
            normalized,
            &filename,
            submitted_at_unix,
        )? {
            changed += 1;
        }
    }
    if changed > 0 {
        state.invalidate_resource(normalized);
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
    let fence = control_epoch(state, normalized).await;
    let filenames = served_filenames(state, index, normalized, version).await?;
    let uploaded = upload_filenames(state, hosted, normalized)?;
    admit_control(state, normalized, fence).await?;
    let mut affected = trash_uploads(state, hosted, volatile, normalized, version, trash)?;
    for filename in filenames {
        if uploaded.contains(&filename) {
            continue;
        }
        state.meta.put_override(
            crate::replication_enabled(state),
            hosted,
            normalized,
            &filename,
            HIDDEN,
            trash.deleted_at_unix,
        )?;
        affected += 1;
    }
    if affected > 0 {
        state.invalidate_resource(normalized);
    }
    Ok(affected)
}

/// Restore a project's files (optionally one version): clear `hidden` overrides so a deleted upstream
/// file reappears, and un-trash soft-deleted uploaded files. Returns how many files reappeared.
///
/// # Errors
/// Returns [`CacheError`] on a store failure.
pub async fn restore_files(
    state: &ServingState,
    hosted: &str,
    normalized: &str,
    version: Option<&str>,
) -> Result<usize, CacheError> {
    let fence = control_epoch(state, normalized).await;
    admit_control(state, normalized, fence).await?;
    let submitted_at_unix = (state.clock)();
    let mut restored = untrash_uploads(state, hosted, normalized, version, submitted_at_unix)?;
    for (filename, kind) in state.meta.list_overrides(hosted, normalized)? {
        if kind != HIDDEN {
            continue;
        }
        if version.is_some_and(|version| !file_matches_version(&filename, version)) {
            continue;
        }
        if state.meta.delete_override(
            crate::replication_enabled(state),
            hosted,
            normalized,
            &filename,
            submitted_at_unix,
        )? {
            restored += 1;
        }
    }
    if restored > 0 {
        state.invalidate_resource(normalized);
    }
    Ok(restored)
}

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
    let Some(detail) = Box::pin(resolve_detail_optional(state, index, normalized, &index.route)).await? else {
        return Ok(ProjectStatus::Active);
    };
    Ok(detail.meta.status())
}

/// Check stored status metadata before serving a content-addressed file download.
///
/// # Errors
/// Returns [`CacheError`] when the store cannot be read.
pub fn download_status(state: &ServingState, index: &Index, filename: &str) -> Result<ProjectStatus, CacheError> {
    let artifact = filename
        .strip_suffix(".metadata")
        .or_else(|| filename.strip_suffix(crate::attestation::PROVENANCE_SUFFIX))
        .unwrap_or(filename);
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
    let Some(detail) = Box::pin(resolve_detail_optional(state, index, normalized, &index.route)).await? else {
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
    state.meta.mutate_uploads(
        crate::replication_enabled(state),
        name,
        normalized,
        "delete-file",
        trash.deleted_at_unix,
        |_filename, bytes| {
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
        },
    )
}

/// Clear the trashed marker off soft-deleted uploaded records, optionally limited to one version, so
/// the files return to every served page. Returns how many records were restored.
fn untrash_uploads(
    state: &ServingState,
    name: &str,
    normalized: &str,
    version: Option<&str>,
    submitted_at_unix: i64,
) -> Result<usize, CacheError> {
    state.meta.mutate_uploads(
        crate::replication_enabled(state),
        name,
        normalized,
        "restore",
        submitted_at_unix,
        |_filename, bytes| {
            let mut uploaded: Uploaded = serde_json::from_slice(bytes)?;
            if uploaded.trashed.is_none() || version.is_some_and(|version| !versions_match(&uploaded.version, version))
            {
                return Ok(UploadMutation::Keep);
            }
            uploaded.trashed = None;
            Ok(UploadMutation::Replace(to_json(&uploaded).into_bytes()))
        },
    )
}

fn yank_uploads(
    state: &ServingState,
    name: &str,
    normalized: &str,
    version: Option<&str>,
    yanked: &Yanked,
    submitted_at_unix: i64,
) -> Result<usize, CacheError> {
    let action = if matches!(yanked, Yanked::No) {
        "unyank"
    } else {
        "withdraw"
    };
    state.meta.mutate_uploads(
        crate::replication_enabled(state),
        name,
        normalized,
        action,
        submitted_at_unix,
        |_filename, bytes| {
            let mut uploaded: Uploaded = serde_json::from_slice(bytes)?;
            if version.is_some_and(|version| !versions_match(&uploaded.version, version))
                || uploaded.file.yanked == *yanked
            {
                return Ok(UploadMutation::Keep);
            }
            uploaded.file.yanked = yanked.clone();
            Ok(UploadMutation::Replace(to_json(&uploaded).into_bytes()))
        },
    )
}
