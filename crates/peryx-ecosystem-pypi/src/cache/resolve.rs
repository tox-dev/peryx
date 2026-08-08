//! Read-path composition: resolve a project's detail and project list across an index's layers.

use std::collections::{BTreeMap, BTreeSet};

use crate::policy::PypiPolicy as _;
use crate::store::CachedIndex;
use crate::store::PypiStore as _;
use crate::upload::Uploaded;
use crate::{CoreMetadata, File, Meta, ProjectDetail, ProjectList, ProjectListEntry, parse_detail};
use peryx_core::path::{is_local_file_url, local_file_url};
use peryx_driver::state::ServingState;
use peryx_identity::{ArtifactDigest, DigestDecision};
use peryx_index::{Index, IndexKind};
use peryx_policy::{FallbackMode, PolicyAction, PolicyDenial, RemoteMetadataMode};
use peryx_upstream::UpstreamClient;

use super::fetch::fetch_and_store;
use super::{CacheError, flight_gate, fresh_cached, project_negative_key, supports_generated_metadata};

/// A resolved project page and the source serial that produced it, when the index has one serial stream.
pub struct DetailPage {
    /// The project data served to the client.
    pub detail: ProjectDetail,
    /// The upstream or local journal serial represented by `detail`.
    pub last_serial: Option<u64>,
    pub(crate) revoked_files_removed: bool,
}

/// Resolve one project's detail across a virtual index's layers, first-match, returning `None` when
/// no layer has the project.
///
/// # Errors
/// Returns [`CacheError`] on a store, parse, or (with no cached fallback) upstream error.
pub async fn resolve_detail(
    state: &ServingState,
    index: &Index,
    project: &str,
    serve_route: &str,
) -> Result<Option<ProjectDetail>, CacheError> {
    let Some(mut page) = resolve_detail_page_with(state, index, project, serve_route, true).await? else {
        return Ok(None);
    };
    filter_revoked_files(state, &mut page.detail)?;
    rewrite_attestation_urls(&mut page.detail, serve_route, index.policy.remote_metadata_mode());
    Ok(Some(page.detail))
}

pub(super) async fn resolve_detail_optional(
    state: &ServingState,
    index: &Index,
    project: &str,
    serve_route: &str,
) -> Result<Option<ProjectDetail>, CacheError> {
    let mut page = resolve_detail_page_with(state, index, project, serve_route, false).await?;
    if let Some(page) = &mut page {
        rewrite_attestation_urls(&mut page.detail, serve_route, index.policy.remote_metadata_mode());
    }
    Ok(page.map(|page| page.detail))
}

/// Resolve a project's detail with the serial represented by that page.
///
/// # Errors
/// Returns [`CacheError`] on policy denial, store failure, invalid cached data, or an upstream error without fallback.
pub async fn resolve_detail_page(
    state: &ServingState,
    index: &Index,
    project: &str,
    serve_route: &str,
) -> Result<Option<DetailPage>, CacheError> {
    let Some(mut page) = resolve_detail_page_with(state, index, project, serve_route, true).await? else {
        return Ok(None);
    };
    page.revoked_files_removed = filter_revoked_files(state, &mut page.detail)?;
    rewrite_attestation_urls(&mut page.detail, serve_route, index.policy.remote_metadata_mode());
    Ok(Some(page))
}

async fn resolve_detail_page_with(
    state: &ServingState,
    index: &Index,
    project: &str,
    serve_route: &str,
    deny_no_fallback_miss: bool,
) -> Result<Option<DetailPage>, CacheError> {
    index.policy.check_project(PolicyAction::Serve, project)?;
    let page = match &index.kind {
        IndexKind::Cached { client, offline } => {
            let Some(mut page) = cached_detail(state, &index.name, &index.route, client, *offline, project).await?
            else {
                return Ok(None);
            };
            rewrite_urls(&mut page.detail, serve_route);
            Some(page)
        }
        IndexKind::Hosted { .. } => {
            let Some(mut detail) = local_detail(state, &index.name, project)? else {
                return Ok(None);
            };
            rewrite_urls(&mut detail, serve_route);
            Some(DetailPage {
                detail,
                last_serial: Some(state.meta.current_serial()?),
                revoked_files_removed: false,
            })
        }
        IndexKind::Virtual { layers, upload } => virtual_detail(
            state,
            index,
            layers,
            *upload,
            project,
            serve_route,
            deny_no_fallback_miss,
        )
        .await?
        .map(|detail| DetailPage {
            detail,
            last_serial: None,
            revoked_files_removed: false,
        }),
    };
    page.map(|mut page| {
        page.detail = index
            .policy
            .apply_detail(PolicyAction::Serve, project, page.detail, Some((state.clock)()))?;
        Ok(page)
    })
    .transpose()
}

/// Resolve eligible members concurrently, then apply the virtual repository's source policy before
/// merging candidates in shadow order. Hidden and yanked overrides from the upload target apply to
/// the resulting view.
async fn virtual_detail(
    state: &ServingState,
    index: &Index,
    layers: &[usize],
    upload: Option<usize>,
    project: &str,
    serve_route: &str,
    deny_no_fallback_miss: bool,
) -> Result<Option<ProjectDetail>, CacheError> {
    let mode = index.policy.fallback_mode();
    let cached_denial = index.policy.check_project(PolicyAction::Cached, project).err();
    let consult_cached = mode != FallbackMode::NoFallback && cached_denial.is_none();
    let ordered: Vec<_> = peryx_index::shadow_order(&state.indexes, layers)
        .into_iter()
        .filter(|&pos| !peryx_index::reaches_cached(&state.indexes, pos) || consult_cached)
        .collect();
    let resolved = futures_util::future::join_all(ordered.iter().map(|&pos| {
        let layer = state.index_at(pos);
        Box::pin(async move {
            Ok(
                resolve_detail_page_with(state, layer, project, serve_route, deny_no_fallback_miss)
                    .await?
                    .map(|page| page.detail),
            )
        })
    }))
    .await;
    let mut details = Vec::new();
    let mut offline_missing = None;
    let mut rate_limited = None;
    for (pos, outcome) in ordered.into_iter().zip(resolved) {
        match outcome {
            Ok(Some(detail)) => details.push((pos, detail)),
            Ok(None) => {}
            Err(err @ CacheError::OfflineMissing(_)) => offline_missing = Some(err),
            Err(err @ CacheError::RateLimited { .. }) => rate_limited = Some(err),
            Err(err) => {
                tracing::warn!(layer = %state.index_at(pos).name, error = ?err, "virtual-index layer unavailable, skipping");
            }
        }
    }
    if mode != FallbackMode::Fallback {
        details.retain(|(_, detail)| !detail.files.is_empty());
    }
    let hosted_found = details
        .iter()
        .any(|(pos, _)| !peryx_index::reaches_cached(&state.indexes, *pos));
    let cached_found = details
        .iter()
        .any(|(pos, _)| peryx_index::reaches_cached(&state.indexes, *pos));
    if mode == FallbackMode::PrivateFirst && hosted_found {
        if cached_found {
            record_collision(state, index, layers, project);
        }
        details.retain(|(pos, _)| !peryx_index::reaches_cached(&state.indexes, *pos));
    }
    if details.is_empty() {
        if let Some(denial) = cached_denial {
            return Err(denial.into());
        }
        if mode == FallbackMode::NoFallback && deny_no_fallback_miss {
            return Err(no_fallback_denial(state, index, layers, project).into());
        }
        if let Some(err) = rate_limited {
            return Err(err);
        }
        if let Some(err) = offline_missing {
            return Err(err);
        }
        return Ok(None);
    }
    let mut files = Vec::new();
    let mut seen = BTreeSet::new();
    let mut versions = BTreeSet::new();
    let mut meta = Meta::default();
    for (_, detail) in details {
        versions.extend(detail.versions);
        // A virtual index guarantees only what its weakest layer does: a layer that cannot promise
        // PEP 700's `versions`/`size` caps the merged page at the base version too.
        if detail.meta.api_version == crate::API_VERSION_BASE {
            meta.api_version = crate::API_VERSION_BASE;
        }
        // Mirror the api_version floor above: a virtual index inherits its most restrictive member, so a
        // member that quarantines a project keeps its files withheld even when a benign member serves them.
        if detail.meta.status().severity() > meta.status().severity() {
            meta.project_status = detail.meta.project_status;
            meta.project_status_reason = detail.meta.project_status_reason;
        }
        for file in detail.files {
            if seen.insert(file.filename.clone()) {
                files.push(file);
            }
        }
    }
    if let Some(pos) = upload {
        apply_overrides(state, &state.index_at(pos).name, project, &mut files)?;
    }
    let mut detail = ProjectDetail {
        meta,
        name: project.to_owned(),
        versions: versions.into_iter().collect(),
        files,
    };
    apply_project_status(&mut detail);
    Ok(Some(detail))
}

fn member_names(state: &ServingState, layers: &[usize], cached: bool) -> String {
    layers
        .iter()
        .filter(|&&pos| peryx_index::reaches_cached(&state.indexes, pos) == cached)
        .map(|&pos| state.index_at(pos).name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn record_collision(state: &ServingState, index: &Index, layers: &[usize], project: &str) {
    let hosted_members = member_names(state, layers, false);
    let cached_members = member_names(state, layers, true);
    tracing::warn!(
        security_event = true,
        event = "policy_decision",
        action = "serve",
        result = "shadowed",
        index = %index.name,
        project,
        fallback_mode = %FallbackMode::PrivateFirst,
        hosted_members,
        cached_members,
        "private-first policy selected hosted project candidates"
    );
}

fn no_fallback_denial(state: &ServingState, index: &Index, layers: &[usize], project: &str) -> PolicyDenial {
    PolicyDenial::new(
        PolicyAction::Cached,
        project,
        None,
        None,
        "virtual-fallback",
        "fallback_mode",
        format!(
            "project {project:?} is missing from hosted members {:?} of virtual repository {:?}; fallback mode {:?} forbids cached members {:?}",
            member_names(state, layers, false),
            index.name,
            FallbackMode::NoFallback.as_str(),
            member_names(state, layers, true),
        ),
    )
}

/// Apply the `hidden`/`yanked` overrides stored on `hosted` to a merged file list.
fn apply_overrides(state: &ServingState, hosted: &str, project: &str, files: &mut Vec<File>) -> Result<(), CacheError> {
    let overrides: BTreeMap<String, String> = state.meta.list_overrides(hosted, project)?.into_iter().collect();
    if overrides.is_empty() {
        return Ok(());
    }
    files.retain(|file| {
        !overrides
            .get(&file.filename)
            .is_some_and(|kind| crate::stream::hidden_override(kind))
    });
    for file in files {
        if let Some(yanked) = overrides
            .get(&file.filename)
            .and_then(|kind| crate::stream::yanked_override(kind))
        {
            file.yanked = yanked;
        }
    }
    Ok(())
}

/// Fetch a cached index's project detail, serving from cache when fresh and revalidating or fetching
/// otherwise. Returns `None` when the project does not exist upstream.
///
/// Concurrent misses for the same page are single-flighted: resolvers such as uv request one
/// project several times in parallel, and each duplicate fetch would download and store a
/// multi-megabyte page again.
async fn cached_detail(
    state: &ServingState,
    name: &str,
    route: &str,
    client: &UpstreamClient,
    offline: bool,
    project: &str,
) -> Result<Option<DetailPage>, CacheError> {
    let key = format!("{name}/{project}");
    if offline {
        return match state.meta.get_index(&key)? {
            Some(record) => Ok(Some(raw_to_page(state, route, &record)?)),
            None => Err(CacheError::OfflineMissing("project page")),
        };
    }
    if let Some(record) = fresh_cached(state, &key)? {
        return Ok(Some(raw_to_page(state, route, &record)?));
    }
    if state.negative_fresh(&project_negative_key(&key)) {
        return Ok(None);
    }

    let gate = flight_gate(state, &key);
    let _guard = gate.lock().await;
    // Whoever held the gate first has stored the page by now; everyone else serves it from cache.
    if let Some(record) = fresh_cached(state, &key)? {
        return Ok(Some(raw_to_page(state, route, &record)?));
    }
    if state.negative_fresh(&project_negative_key(&key)) {
        return Ok(None);
    }

    let result = fetch_and_store(state, &key, name, project, client).await;
    state.cache.forget_flight(&key);
    match result? {
        Some(record) => Ok(Some(raw_to_page(state, route, &record)?)),
        None => Ok(None),
    }
}

fn raw_to_page(state: &ServingState, route: &str, record: &CachedIndex) -> Result<DetailPage, CacheError> {
    Ok(DetailPage {
        detail: raw_to_detail(state, route, record)?,
        last_serial: record.last_serial,
        revoked_files_removed: false,
    })
}

fn filter_revoked_files(state: &ServingState, detail: &mut ProjectDetail) -> Result<bool, CacheError> {
    if !super::has_active_revocations(state)? {
        return Ok(false);
    }
    let mut removed = false;
    let mut files = Vec::with_capacity(detail.files.len());
    for file in std::mem::take(&mut detail.files) {
        let digest = file
            .hashes
            .get("sha256")
            .and_then(|sha256| ArtifactDigest::from_sha256(sha256).ok());
        let revoked = match digest {
            Some(digest) => state.revocations.decision(&digest)? == DigestDecision::Revoked,
            None => false,
        };
        if revoked {
            removed = true;
        } else {
            files.push(file);
        }
    }
    detail.files = files;
    Ok(removed)
}

/// Turn a raw cached page into the detail served on `route`: parse, drop unverifiable metadata
/// claims, and point content-addressable files at peryx's own file route.
pub fn raw_to_detail(state: &ServingState, route: &str, record: &CachedIndex) -> Result<ProjectDetail, CacheError> {
    let parsed = parse_detail(&record.body)?;
    let known_metadata = known_metadata(state, &parsed.files)?;
    let files = parsed
        .files
        .into_iter()
        .map(|file| present_file(file, route, &known_metadata))
        .collect();
    let mut detail = ProjectDetail {
        meta: parsed.meta,
        name: parsed.name,
        versions: parsed.versions,
        files,
    };
    apply_project_status(&mut detail);
    Ok(detail)
}

fn apply_project_status(detail: &mut ProjectDetail) {
    if !detail.meta.status().offers_downloads() {
        detail.files.clear();
    }
}

/// The pure serving transform for one file: peryx URL for content-addressable files, metadata
/// claims kept only when verifiable by digest.
fn present_file(mut file: File, route: &str, known_metadata: &BTreeMap<String, String>) -> File {
    let Some(sha256) = file.hashes.get("sha256").cloned() else {
        file.clear_metadata();
        return file;
    };
    if !matches!(file.metadata(), CoreMetadata::Hashes(hashes) if hashes.contains_key("sha256")) {
        file.clear_metadata();
    }
    if file.metadata().is_absent()
        && supports_generated_metadata(&file.filename)
        && let Some(metadata) = known_metadata.get(&sha256)
    {
        file.set_metadata(CoreMetadata::Hashes(std::collections::BTreeMap::from([(
            "sha256".to_owned(),
            metadata.clone(),
        )])));
    }
    if !is_local_file_url(route, &file.url) {
        file.url = local_file_url(route, &sha256, &file.filename);
    }
    // The URL now points at peryx's route, which serves the blob but never the detached `.asc`
    // sibling, so drop any inherited gpg-sig rather than advertise a signature peryx cannot serve.
    file.gpg_sig = None;
    file
}

pub(super) fn known_metadata(state: &ServingState, files: &[File]) -> Result<BTreeMap<String, String>, CacheError> {
    let artifact_sha256s = files
        .iter()
        .filter(|file| supports_generated_metadata(&file.filename) && file.metadata().is_absent())
        .filter_map(|file| file.hashes.get("sha256").map(String::as_str));
    Ok(state.meta.get_metadata_digests(artifact_sha256s)?)
}

/// Build a hosted (uploaded) project's detail from its stored file records. Yank markers are kept, so
/// yanked files stay downloadable but are skipped by resolvers; soft-deleted files are dropped, so a
/// project whose files are all trashed reads as absent.
pub(super) fn local_detail(
    state: &ServingState,
    name: &str,
    project: &str,
) -> Result<Option<ProjectDetail>, CacheError> {
    let entries = state.meta.list_upload_entries(name, project)?;
    if entries.is_empty() {
        return Ok(None);
    }
    let mut files = Vec::with_capacity(entries.len());
    let mut versions = BTreeSet::new();
    for (_filename, bytes) in entries {
        let uploaded: Uploaded = serde_json::from_slice(&bytes)?;
        if uploaded.trashed.is_some() {
            continue;
        }
        versions.insert(uploaded.version);
        files.push(uploaded.file);
    }
    if files.is_empty() {
        return Ok(None);
    }
    let mut detail = ProjectDetail {
        meta: Meta::default(),
        name: project.to_owned(),
        versions: versions.into_iter().collect(),
        files,
    };
    apply_project_status(&mut detail);
    Ok(Some(detail))
}

/// Point every content-addressable file at peryx's own file route on `route`.
pub(super) fn rewrite_urls(detail: &mut ProjectDetail, route: &str) {
    for file in &mut detail.files {
        if let Some(sha256) = file.hashes.get("sha256") {
            file.url = local_file_url(route, sha256, &file.filename);
        }
    }
}

fn rewrite_attestation_urls(detail: &mut ProjectDetail, route: &str, mode: RemoteMetadataMode) {
    if mode == RemoteMetadataMode::Direct {
        return;
    }
    for file in &mut detail.files {
        let Some(sha256) = file.sha256().map(str::to_owned) else {
            continue;
        };
        if file.provenance.secure_url().is_some() {
            file.provenance =
                crate::Provenance::Url(local_file_url(route, &sha256, &format!("{}.provenance", file.filename)));
        }
    }
}

/// The project names peryx has observed on `index`, unioned across a virtual index's layers.
///
/// # Errors
/// Returns [`CacheError`] if a store read fails.
pub fn resolve_list(state: &ServingState, index: &Index) -> Result<ProjectList, CacheError> {
    let mut names = BTreeSet::new();
    collect_projects(state, index, &mut names)?;
    Ok(index.policy.apply_list(ProjectList {
        meta: Meta::default(),
        projects: names.into_iter().map(|name| ProjectListEntry { name }).collect(),
    }))
}

/// Return the stable serial for a project list, omitting indexes whose layers have no combined serial model.
///
/// # Errors
/// Returns [`CacheError`] when the local serial cannot be read.
pub fn list_serial(state: &ServingState, index: &Index) -> Result<Option<u64>, CacheError> {
    match &index.kind {
        IndexKind::Hosted { .. } => Ok(Some(state.meta.current_serial()?)),
        IndexKind::Cached { .. } | IndexKind::Virtual { .. } => Ok(None),
    }
}

fn collect_projects(state: &ServingState, index: &Index, names: &mut BTreeSet<String>) -> Result<(), CacheError> {
    match &index.kind {
        IndexKind::Cached { .. } | IndexKind::Hosted { .. } => {
            names.extend(state.meta.list_projects(&index.name)?);
        }
        IndexKind::Virtual { layers, .. } => {
            for &pos in layers {
                collect_projects(state, state.index_at(pos), names)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/cache/resolve/tests.rs"]
mod tests;
