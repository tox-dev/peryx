use std::collections::{BTreeMap, BTreeSet, HashSet};

use crate::quota::PendingQuota;
use crate::store::PypiStore as _;
use crate::store::{Guard, PromotedRelease};
use crate::upload::{self, PreparedUpload, TrashInfo, Uploaded};
use crate::{ProjectStatus, Yanked, file_matches_version, parse_distribution_filename, to_json, versions_match};
use peryx_core::path::local_artifact_url;
use peryx_driver::state::ServingState;
use peryx_index::{Index, IndexKind};

use super::CacheError;
use super::resolve::resolve_detail_optional;

/// A metadata control routes through the project's ownership authority: the normalized project name is
/// the authority key, so every PEP 503 name variant fences on one epoch. The control snapshots that
/// epoch before it reads, then leases it through the metadata commit.
///
/// Snapshot the committed epoch a control will finalize under. `0` is unfenced only when the process has
/// no ownership group; a configured group must lease a nonzero committed epoch.
async fn control_epoch(state: &ServingState, authority: &str) -> u64 {
    state.committed_authority_epoch(authority).await
}

async fn commit_control<T>(
    state: &ServingState,
    authority: &str,
    fence: u64,
    mutation: impl FnOnce(&ControlLease<'_>) -> Result<T, CacheError>,
) -> Result<T, CacheError> {
    let lease = match state.begin_authority_epoch_write(authority, fence).await {
        Ok(Some(lease)) => Some(lease),
        Ok(None) if state.ownership_authority().is_none() => None,
        Ok(None) | Err(_) => return Err(CacheError::AuthoritySuperseded),
    };
    let lease = ControlLease { state, lease };
    let result = mutation(&lease);
    lease.finish().await;
    result
}

struct ControlLease<'a> {
    state: &'a ServingState,
    lease: Option<peryx_ha::AuthorityWriteLease>,
}

impl ControlLease<'_> {
    fn check(&self) -> Result<(), CacheError> {
        if self
            .lease
            .as_ref()
            .is_some_and(|lease| !lease.admits((self.state.clock)()))
        {
            return Err(CacheError::AuthoritySuperseded);
        }
        Ok(())
    }

    async fn finish(self) {
        if let Some(lease) = self.lease
            && let Err(error) = self.state.finish_authority_epoch_write(&lease).await
        {
            tracing::warn!(%error, authority = lease.authority, "authority write lease release failed");
        }
    }
}

/// Persist a prepared upload into the hosted store `name`: commit the staged blobs, record the file
/// and its project, and bump the serial. Returns `false` for a same-bytes duplicate.
///
/// The publish fences on the project's ownership authority like every other mutation: it snapshots the
/// committed epoch, commits the blobs, then leases the epoch through the record write.
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
    webhook: Option<peryx_storage::meta::WebhookEventIntent>,
) -> Result<StoredUpload, CacheError> {
    let publish = upload::stage_publish(&state.blobs, prepared).await?;
    let published = commit_control(state, project, fence, |lease| {
        lease.check()?;
        upload::commit_publish(
            &state.meta,
            name,
            publish,
            quota,
            crate::replication_enabled(state),
            webhook,
        )
        .map_err(CacheError::from)
    })
    .await?;
    for (digest, size) in &published.placements {
        state.record_home_placement(digest.as_str(), *size, fence);
    }
    state.record_operation_trace(peryx_driver::state::OperationKind::Publish, fence);
    if published.stored {
        super::invalidate_project(state, name, project);
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
        // The bundle is copied onto the target publication, so its marker names the target's route.
        if let crate::Provenance::Url(_) = uploaded.file.provenance {
            uploaded.file.provenance = crate::Provenance::Url(format!(
                "{}{}",
                uploaded.file.url,
                crate::attestation::PROVENANCE_SUFFIX
            ));
        }
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
    let release = PromotedRelease {
        source,
        index: target,
        normalized,
        display: &display,
        records: &records,
        blob_sizes: &blob_sizes,
        submitted_at_unix: (state.clock)(),
    };
    let promoted = commit_control(state, normalized, fence, |lease| {
        lease.check()?;
        state
            .meta
            .promote_files_checked(crate::replication_enabled(state), &release, promote_conflict)
    })
    .await?;
    if promoted > 0 {
        super::invalidate_project(state, target, normalized);
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

/// Set or clear the yank state of a project's files as served by `index`.
///
/// Uploaded files get their stored record rewritten; read-only upstream files get the yank field of
/// their override record on `hosted` rewritten, which leaves an administrative hide in place.
/// Returns how many files changed.
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
    set_yanked_with_webhook(state, index, hosted, normalized, version, yanked, |_| None).await
}

pub async fn set_yanked_with_webhook(
    state: &ServingState,
    index: &Index,
    hosted: &str,
    normalized: &str,
    version: Option<&str>,
    yanked: Yanked,
    webhook: impl FnOnce(usize) -> Option<peryx_storage::meta::WebhookEventIntent>,
) -> Result<usize, CacheError> {
    let fence = control_epoch(state, normalized).await;
    let uploaded = upload_filenames(state, hosted, normalized)?;
    let served = served_filenames(state, index, normalized, version).await?;
    // A hidden file is served by no layer, so it has to be named explicitly for a yank to reach it;
    // the set collapses the layer that still serves a filename hidden on `hosted`.
    let override_filenames = served
        .into_iter()
        .chain(hidden_filenames(state, hosted, normalized, version)?)
        .filter(|filename| !uploaded.contains(filename))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let submitted_at_unix = (state.clock)();
    let changed = commit_control(state, normalized, fence, |lease| {
        lease.check()?;
        let action = if matches!(yanked, Yanked::No) {
            "unyank"
        } else {
            "withdraw"
        };
        crate::store::mutate_uploads_and_overrides(
            &state.meta,
            crate::store::UploadMutationPlan {
                outbox: crate::replication_enabled(state),
                index: hosted,
                normalized,
                action,
                submitted_at_unix,
                override_filenames: &override_filenames,
                override_mutation: crate::store::OverrideMutation::Yanked(&yanked),
            },
            || lease.check(),
            |_filename, bytes| -> Result<Option<Vec<u8>>, CacheError> {
                let mut uploaded: Uploaded = serde_json::from_slice(bytes)?;
                if version.is_some_and(|version| !versions_match(&uploaded.version, version))
                    || uploaded.file.yanked == yanked
                {
                    return Ok(None);
                }
                uploaded.file.yanked = yanked.clone();
                Ok(Some(to_json(&uploaded).into_bytes()))
            },
            webhook,
        )
    })
    .await?;
    if changed > 0 {
        super::invalidate_project(state, hosted, normalized);
    }
    Ok(changed)
}

/// The provenance a soft-delete records on each file it trashes, threaded from the delete request.
#[derive(Clone, Copy)]
pub struct TrashContext<'a> {
    pub deleted_at_unix: i64,
    pub actor: Option<&'a str>,
    pub reason: Option<&'a str>,
}

#[derive(Clone, Copy)]
pub struct RemovalContext<'a> {
    pub volatile: bool,
    pub trash: TrashContext<'a>,
}

/// Remove a project's files as served by `index`.
///
/// Uploaded files are soft-deleted (requires `volatile`): the record is marked trashed and its blob
/// kept, so the file drops out of every served page but stays recoverable until a restore or a purge.
/// Read-only upstream files get the hidden field of their override record on `hosted` set, which
/// leaves any yank state the file already carried intact. Returns how many files were affected.
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
    remove_files_with_webhook(
        state,
        index,
        hosted,
        normalized,
        version,
        RemovalContext { volatile, trash },
        |_| None,
    )
    .await
}

pub async fn remove_files_with_webhook(
    state: &ServingState,
    index: &Index,
    hosted: &str,
    normalized: &str,
    version: Option<&str>,
    removal: RemovalContext<'_>,
    webhook: impl FnOnce(usize) -> Option<peryx_storage::meta::WebhookEventIntent>,
) -> Result<usize, CacheError> {
    let fence = control_epoch(state, normalized).await;
    let filenames = served_filenames(state, index, normalized, version).await?;
    let uploaded = upload_filenames(state, hosted, normalized)?;
    let override_filenames = filenames
        .into_iter()
        .filter(|filename| !uploaded.contains(filename))
        .collect::<Vec<_>>();
    let affected = commit_control(state, normalized, fence, |lease| {
        lease.check()?;
        crate::store::mutate_uploads_and_overrides(
            &state.meta,
            crate::store::UploadMutationPlan {
                outbox: crate::replication_enabled(state),
                index: hosted,
                normalized,
                action: "delete-file",
                submitted_at_unix: removal.trash.deleted_at_unix,
                override_filenames: &override_filenames,
                override_mutation: crate::store::OverrideMutation::Hidden(true),
            },
            || lease.check(),
            |_filename, bytes| -> Result<Option<Vec<u8>>, CacheError> {
                let mut uploaded: Uploaded = serde_json::from_slice(bytes)?;
                if version.is_some_and(|version| !versions_match(&uploaded.version, version))
                    || uploaded.trashed.is_some()
                {
                    return Ok(None);
                }
                if !removal.volatile {
                    return Err(CacheError::NotVolatile);
                }
                uploaded.trashed = Some(TrashInfo {
                    deleted_at_unix: removal.trash.deleted_at_unix,
                    actor: removal.trash.actor.map(str::to_owned),
                    reason: removal.trash.reason.map(str::to_owned),
                });
                Ok(Some(to_json(&uploaded).into_bytes()))
            },
            webhook,
        )
    })
    .await?;
    if affected > 0 {
        super::invalidate_project(state, hosted, normalized);
    }
    Ok(affected)
}

/// Restore a project's files, optionally one version.
///
/// Clears the hidden field of every hidden override so a deleted upstream file reappears - still
/// yanked when it was yanked before the delete - and un-trashes soft-deleted uploaded files. Returns
/// how many files reappeared.
///
/// # Errors
/// Returns [`CacheError`] on a store failure.
pub async fn restore_files(
    state: &ServingState,
    hosted: &str,
    normalized: &str,
    version: Option<&str>,
) -> Result<usize, CacheError> {
    restore_files_with_webhook(state, hosted, normalized, version, |_| None).await
}

pub async fn restore_files_with_webhook(
    state: &ServingState,
    hosted: &str,
    normalized: &str,
    version: Option<&str>,
    webhook: impl FnOnce(usize) -> Option<peryx_storage::meta::WebhookEventIntent>,
) -> Result<usize, CacheError> {
    let fence = control_epoch(state, normalized).await;
    let override_filenames = hidden_filenames(state, hosted, normalized, version)?;
    let submitted_at_unix = (state.clock)();
    let restored = commit_control(state, normalized, fence, |lease| {
        lease.check()?;
        crate::store::mutate_uploads_and_overrides(
            &state.meta,
            crate::store::UploadMutationPlan {
                outbox: crate::replication_enabled(state),
                index: hosted,
                normalized,
                action: "restore",
                submitted_at_unix,
                override_filenames: &override_filenames,
                override_mutation: crate::store::OverrideMutation::Hidden(false),
            },
            || lease.check(),
            |_filename, bytes| -> Result<Option<Vec<u8>>, CacheError> {
                let mut uploaded: Uploaded = serde_json::from_slice(bytes)?;
                if uploaded.trashed.is_none()
                    || version.is_some_and(|version| !versions_match(&uploaded.version, version))
                {
                    return Ok(None);
                }
                uploaded.trashed = None;
                Ok(Some(to_json(&uploaded).into_bytes()))
            },
            webhook,
        )
    })
    .await?;
    if restored > 0 {
        super::invalidate_project(state, hosted, normalized);
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

/// The files a delete withdrew from every served page. A yank still has to reach them: a hide and a
/// yank are independent states, so an operator must be able to unyank a file while it is hidden, and
/// a restore has to return it carrying whatever yank it ended up with.
fn hidden_filenames(
    state: &ServingState,
    hosted: &str,
    normalized: &str,
    version: Option<&str>,
) -> Result<Vec<String>, CacheError> {
    Ok(state
        .meta
        .list_overrides(hosted, normalized)?
        .into_iter()
        .filter(|(filename, record)| {
            crate::store::FileOverride::decode(record).is_some_and(|record| record.hidden)
                && version.is_none_or(|version| file_matches_version(filename, version))
        })
        .map(|(filename, _)| filename)
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
