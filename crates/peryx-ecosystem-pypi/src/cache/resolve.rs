//! Read-path composition: resolve a project's detail and project list across an index's layers.

use std::collections::{BTreeSet, HashSet};

use crate::policy::PypiPolicy as _;
use crate::store::CachedIndex;
use crate::store::PypiStore as _;
use crate::upload::Uploaded;
use crate::{CoreMetadata, File, Meta, ProjectDetail, ProjectList, ProjectListEntry, parse_detail};
use peryx_core::path::{is_local_file_url, local_file_url};
use peryx_driver::state::ServingState;
use peryx_index::{Index, IndexKind};
use peryx_policy::PolicyAction;
use peryx_upstream::UpstreamClient;

use super::fetch::fetch_and_store;
use super::{CacheError, flight_gate, fresh_cached, project_negative_key, supports_generated_metadata};

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
    index.policy.check_project(PolicyAction::Serve, project)?;
    let detail = match &index.kind {
        IndexKind::Cached { client, offline } => {
            let Some(mut detail) = cached_detail(state, &index.name, &index.route, client, *offline, project).await?
            else {
                return Ok(None);
            };
            rewrite_urls(&mut detail, serve_route);
            Some(detail)
        }
        IndexKind::Hosted { .. } => {
            let Some(mut detail) = local_detail(state, &index.name, project)? else {
                return Ok(None);
            };
            rewrite_urls(&mut detail, serve_route);
            Some(detail)
        }
        IndexKind::Virtual { layers, upload } => virtual_detail(state, layers, *upload, project, serve_route).await?,
    };
    detail
        .map(|detail| {
            index
                .policy
                .apply_detail(PolicyAction::Serve, project, detail)
                .map_err(CacheError::from)
        })
        .transpose()
}

/// Merge the layers of a virtual index: first match per filename wins, versions are unioned. Overrides
/// recorded on the virtual index's upload layer then apply: `hidden` files drop out of the page and
/// `yanked` files carry the PEP 592 marker, which is how read-only upstream files are yanked or
/// removed without touching the cache.
///
/// Cached layers merge last however the operator ordered `layers`, so a hosted file always shadows a
/// same-named upstream one. That ordering is the dependency-confusion defense, and leaving it to the
/// configured order would make it an operator's mistake to lose.
async fn virtual_detail(
    state: &ServingState,
    layers: &[usize],
    upload: Option<usize>,
    project: &str,
    serve_route: &str,
) -> Result<Option<ProjectDetail>, CacheError> {
    // Layers resolve concurrently; `shadow_order` fixes the merge precedence.
    let ordered = peryx_index::shadow_order(&state.indexes, layers);
    let resolved = futures_util::future::join_all(ordered.iter().map(|&pos| {
        let layer = state.index_at(pos);
        Box::pin(resolve_detail(state, layer, project, serve_route))
    }))
    .await;
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    let mut versions = BTreeSet::new();
    let mut meta = Meta::default();
    let mut found = false;
    let mut offline_missing = None;
    let mut rate_limited = None;
    for (pos, outcome) in ordered.into_iter().zip(resolved) {
        // A layer being unavailable (a down upstream with a cold cache) must not break the others.
        let detail = match outcome {
            Ok(detail) => detail,
            Err(err @ CacheError::OfflineMissing(_)) => {
                offline_missing = Some(err);
                continue;
            }
            // A saturated upstream cap is transient. If no other layer serves the project, propagate it
            // as a retryable error rather than skipping the layer and reporting the project as missing.
            Err(err @ CacheError::RateLimited { .. }) => {
                rate_limited = Some(err);
                continue;
            }
            Err(err) => {
                let layer = state.index_at(pos);
                tracing::warn!(layer = %layer.name, error = ?err, "virtual-index layer unavailable, skipping");
                continue;
            }
        };
        if let Some(detail) = detail {
            found = true;
            versions.extend(detail.versions);
            // A virtual index guarantees only what its weakest layer does: a layer that cannot promise
            // PEP 700's `versions`/`size` caps the merged page at the base version too.
            if detail.meta.api_version == crate::API_VERSION_BASE {
                meta.api_version = crate::API_VERSION_BASE;
            }
            if meta.project_status.is_none() && detail.meta.project_status.is_some() {
                meta.project_status = detail.meta.project_status;
                meta.project_status_reason = detail.meta.project_status_reason;
            }
            for file in detail.files {
                if seen.insert(file.filename.clone()) {
                    files.push(file);
                }
            }
        }
    }
    if !found {
        if let Some(err) = rate_limited {
            return Err(err);
        }
        if let Some(err) = offline_missing {
            return Err(err);
        }
        return Ok(None);
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

/// Apply the `hidden`/`yanked` overrides stored on `hosted` to a merged file list.
fn apply_overrides(state: &ServingState, hosted: &str, project: &str, files: &mut Vec<File>) -> Result<(), CacheError> {
    let overrides: std::collections::HashMap<String, String> =
        state.meta.list_overrides(hosted, project)?.into_iter().collect();
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
) -> Result<Option<ProjectDetail>, CacheError> {
    let key = format!("{name}/{project}");
    if offline {
        return match state.meta.get_index(&key)? {
            Some(record) => Ok(Some(raw_to_detail(state, route, &record)?)),
            None => Err(CacheError::OfflineMissing("project page")),
        };
    }
    if let Some(record) = fresh_cached(state, &key)? {
        return Ok(Some(raw_to_detail(state, route, &record)?));
    }
    if state.negative_fresh(&project_negative_key(&key)) {
        return Ok(None);
    }

    let gate = flight_gate(state, &key);
    let _guard = gate.lock().await;
    // Whoever held the gate first has stored the page by now; everyone else serves it from cache.
    if let Some(record) = fresh_cached(state, &key)? {
        return Ok(Some(raw_to_detail(state, route, &record)?));
    }
    if state.negative_fresh(&project_negative_key(&key)) {
        return Ok(None);
    }

    let result = fetch_and_store(state, &key, name, project, client).await;
    state.cache.forget_flight(&key);
    match result? {
        Some(record) => Ok(Some(raw_to_detail(state, route, &record)?)),
        None => Ok(None),
    }
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
fn present_file(mut file: File, route: &str, known_metadata: &std::collections::HashMap<String, String>) -> File {
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

pub(super) fn known_metadata(
    state: &ServingState,
    files: &[File],
) -> Result<std::collections::HashMap<String, String>, CacheError> {
    let artifact_sha256s = files
        .iter()
        .filter(|file| supports_generated_metadata(&file.filename) && file.metadata().is_absent())
        .filter_map(|file| file.hashes.get("sha256").map(String::as_str));
    Ok(state.meta.get_metadata_digests(artifact_sha256s)?)
}

/// Build a hosted (uploaded) project's detail from its stored file records. Yank markers are kept, so
/// yanked files stay downloadable but are skipped by resolvers.
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
        versions.insert(uploaded.version);
        files.push(uploaded.file);
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
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use crate::{Provenance, Yanked};

    use super::*;

    #[test]
    fn test_present_file_advertises_cached_generated_metadata() {
        let artifact = "a".repeat(64);
        let metadata = "b".repeat(64);
        let file = File {
            filename: "pkg-1.0-py3-none-any.whl".to_owned(),
            url: "https://files.example/pkg-1.0-py3-none-any.whl".to_owned(),
            hashes: BTreeMap::from([("sha256".to_owned(), artifact.clone())]),
            requires_python: None,
            size: None,
            upload_time: None,
            yanked: Yanked::No,
            core_metadata: CoreMetadata::Absent,
            dist_info_metadata: CoreMetadata::Absent,
            gpg_sig: None,
            provenance: Provenance::default(),
        };

        let file = present_file(file, "pypi", &HashMap::from([(artifact.clone(), metadata.clone())]));

        assert_eq!(file.url, local_file_url("pypi", &artifact, "pkg-1.0-py3-none-any.whl"));
        assert!(matches!(file.metadata(), CoreMetadata::Hashes(hashes) if hashes["sha256"] == metadata));
    }

    #[test]
    fn test_present_file_content_addresses_when_sha256_accompanies_other_hashes() {
        let sha256 = "a".repeat(64);
        let file = File {
            filename: "pkg-1.0-py3-none-any.whl".to_owned(),
            url: "https://files.example/pkg-1.0-py3-none-any.whl".to_owned(),
            hashes: BTreeMap::from([
                ("md5".to_owned(), "deadbeef".to_owned()),
                ("sha256".to_owned(), sha256.clone()),
            ]),
            requires_python: None,
            size: None,
            upload_time: None,
            yanked: Yanked::No,
            core_metadata: CoreMetadata::Absent,
            dist_info_metadata: CoreMetadata::Absent,
            gpg_sig: None,
            provenance: Provenance::default(),
        };

        let file = present_file(file, "pypi", &HashMap::new());

        assert_eq!(file.url, local_file_url("pypi", &sha256, "pkg-1.0-py3-none-any.whl"));
        assert_eq!(file.hashes.get("md5").map(String::as_str), Some("deadbeef"));
    }

    #[test]
    fn test_present_file_drops_gpg_sig_once_url_points_at_peryx() {
        let sha256 = "a".repeat(64);
        let file = File {
            filename: "pkg-1.0-py3-none-any.whl".to_owned(),
            url: "https://files.example/pkg-1.0-py3-none-any.whl".to_owned(),
            hashes: BTreeMap::from([("sha256".to_owned(), sha256.clone())]),
            requires_python: None,
            size: None,
            upload_time: None,
            yanked: Yanked::No,
            core_metadata: CoreMetadata::Absent,
            dist_info_metadata: CoreMetadata::Absent,
            gpg_sig: Some(true),
            provenance: Provenance::default(),
        };

        let file = present_file(file, "pypi", &HashMap::new());

        assert_eq!(file.url, local_file_url("pypi", &sha256, "pkg-1.0-py3-none-any.whl"));
        assert_eq!(file.gpg_sig, None);
    }

    #[test]
    fn test_present_file_keeps_gpg_sig_when_url_stays_upstream() {
        let file = File {
            filename: "pkg-1.0.tar.gz".to_owned(),
            url: "https://files.example/pkg-1.0.tar.gz".to_owned(),
            hashes: BTreeMap::from([("md5".to_owned(), "deadbeef".to_owned())]),
            requires_python: None,
            size: None,
            upload_time: None,
            yanked: Yanked::No,
            core_metadata: CoreMetadata::Absent,
            dist_info_metadata: CoreMetadata::Absent,
            gpg_sig: Some(true),
            provenance: Provenance::default(),
        };

        let file = present_file(file, "pypi", &HashMap::new());

        assert_eq!(file.url, "https://files.example/pkg-1.0.tar.gz");
        assert_eq!(file.gpg_sig, Some(true));
    }
}
