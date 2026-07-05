//! The `PyPI` search-document mapping: how cached/hosted/virtual `PyPI` project metadata becomes the
//! neutral [`PackageDocument`]s the [`PackageSearch`](velodex_http::search::PackageSearch) index stores.
//!
//! The tantivy index, its schema, and querying are ecosystem-neutral and live in
//! [`velodex_http::search`]; only the walk from an index's stored records to a project's searchable
//! text is PyPI-shaped, so it sits behind the [`PackageIndexer`] seam here. A future ecosystem
//! supplies its own indexer.

use std::collections::{BTreeSet, HashSet};

use crate::policy::PypiPolicy;
use crate::{CoreMetadata, CoreMetadataDoc, File, Meta, ProjectDetail, ProjectStatus, parse_detail, parse_metadata};
use velodex_policy::PolicyAction;
use velodex_storage::blob::Digest;
use velodex_storage::meta::CachedIndex;

use crate::upload::Uploaded;
use velodex_http::path_safety::local_file_url;
use velodex_http::search::{
    INDEXED_TEXT_BYTES, PackageDocument, PackageIndexer, PackageSource, SearchError, truncate_to_chars,
};
use velodex_http::state::{AppState, Index, IndexKind};

/// Produces `PyPI` search documents for the neutral search index.
#[derive(Debug, Clone, Copy, Default)]
pub struct PypiIndexer;

impl PackageIndexer for PypiIndexer {
    fn documents(&self, state: &AppState) -> Result<Vec<PackageDocument>, SearchError> {
        let mut documents = Vec::new();
        for index in &state.indexes {
            let mut projects = BTreeSet::new();
            collect_projects(state, index, &mut projects)?;
            for normalized in projects {
                if let Some(package) = package_document(state, index, &normalized)? {
                    documents.push(package);
                }
            }
        }
        Ok(documents)
    }
}

fn collect_projects(state: &AppState, index: &Index, projects: &mut BTreeSet<String>) -> Result<(), SearchError> {
    match &index.kind {
        IndexKind::Cached { .. } => {
            state.meta.scan_index_records(|key, _value| {
                if let Some(project) = project_key(key, &index.name) {
                    projects.insert(project.to_owned());
                }
                Ok(())
            })?;
        }
        IndexKind::Hosted { .. } => {
            state.meta.scan_upload_records(|key, _value| {
                if let Some((project, _filename)) = upload_key(key, &index.name) {
                    projects.insert(project.to_owned());
                }
                Ok(())
            })?;
        }
        IndexKind::Virtual { layers, .. } => {
            for &position in layers {
                collect_projects(state, state.index_at(position), projects)?;
            }
        }
    }
    Ok(())
}

fn package_document(state: &AppState, index: &Index, normalized: &str) -> Result<Option<PackageDocument>, SearchError> {
    let detail = cached_detail(state, index, normalized, &index.route)?;
    if detail.files.is_empty() {
        return Ok(None);
    }
    let source = package_source(state, index, normalized)?;
    let metadata = metadata_doc(state, &detail)?;
    let display_name = metadata
        .as_ref()
        .map(|doc| doc.name.as_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(if detail.name.is_empty() {
            normalized
        } else {
            &detail.name
        })
        .to_owned();
    let summary = metadata.as_ref().and_then(|doc| doc.summary.clone());
    Ok(Some(PackageDocument {
        text: search_text(&display_name, normalized, &detail, metadata.as_ref()),
        display_name,
        normalized_name: normalized.to_owned(),
        route: index.route.clone(),
        index: index.name.clone(),
        source,
        summary,
    }))
}

fn cached_detail(
    state: &AppState,
    index: &Index,
    normalized: &str,
    serve_route: &str,
) -> Result<ProjectDetail, SearchError> {
    let detail = match &index.kind {
        IndexKind::Cached { .. } => mirror_detail(state, index, normalized, serve_route),
        IndexKind::Hosted { .. } => local_detail(state, &index.name, normalized, serve_route),
        IndexKind::Virtual { layers, upload } => overlay_detail(state, layers, *upload, normalized, serve_route),
    }?;
    Ok(index
        .policy
        .apply_detail(PolicyAction::Serve, normalized, detail)
        .unwrap_or_else(|_| empty_detail(normalized)))
}

fn mirror_detail(
    state: &AppState,
    index: &Index,
    normalized: &str,
    serve_route: &str,
) -> Result<ProjectDetail, SearchError> {
    let Some(record) = state.meta.get_index(&format!("{}/{normalized}", index.name))? else {
        return Ok(empty_detail(normalized));
    };
    detail_from_record(serve_route, &record)
}

fn detail_from_record(route: &str, record: &CachedIndex) -> Result<ProjectDetail, SearchError> {
    let parsed = parse_detail(&record.body).map_err(|err| SearchError::Indexer(err.to_string()))?;
    let files = parsed.files.into_iter().map(|file| present_file(file, route)).collect();
    let mut detail = ProjectDetail {
        meta: parsed.meta,
        name: parsed.name,
        versions: parsed.versions,
        files,
    };
    apply_project_status(&mut detail);
    Ok(detail)
}

fn present_file(mut file: File, route: &str) -> File {
    let Some(sha256) = file.hashes.get("sha256").cloned() else {
        file.clear_metadata();
        return file;
    };
    if !matches!(file.metadata(), CoreMetadata::Hashes(hashes) if hashes.contains_key("sha256")) {
        file.clear_metadata();
    }
    if !file.url.starts_with('/') {
        file.url = local_file_url(route, &sha256, &file.filename);
    }
    file
}

fn local_detail(
    state: &AppState,
    name: &str,
    normalized: &str,
    serve_route: &str,
) -> Result<ProjectDetail, SearchError> {
    let entries = state.meta.list_upload_entries(name, normalized)?;
    if entries.is_empty() {
        return Ok(empty_detail(normalized));
    }
    let mut files = Vec::with_capacity(entries.len());
    let mut versions = BTreeSet::new();
    for (_filename, bytes) in entries {
        let mut uploaded: Uploaded = serde_json::from_slice(&bytes)?;
        versions.insert(uploaded.version);
        if let Some(sha256) = uploaded.file.hashes.get("sha256") {
            uploaded.file.url = local_file_url(serve_route, sha256, &uploaded.file.filename);
        }
        files.push(uploaded.file);
    }
    let mut detail = ProjectDetail {
        meta: Meta::default(),
        name: normalized.to_owned(),
        versions: versions.into_iter().collect(),
        files,
    };
    apply_project_status(&mut detail);
    Ok(detail)
}

fn overlay_detail(
    state: &AppState,
    layers: &[usize],
    upload: Option<usize>,
    normalized: &str,
    serve_route: &str,
) -> Result<ProjectDetail, SearchError> {
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    let mut versions = BTreeSet::new();
    let mut meta = Meta::default();
    for &position in layers {
        let detail = cached_detail(state, state.index_at(position), normalized, serve_route)?;
        if detail.files.is_empty() {
            continue;
        }
        versions.extend(detail.versions);
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
    if let Some(position) = upload {
        apply_overrides(state, &state.index_at(position).name, normalized, &mut files)?;
    }
    let mut detail = ProjectDetail {
        meta,
        name: normalized.to_owned(),
        versions: versions.into_iter().collect(),
        files,
    };
    apply_project_status(&mut detail);
    Ok(detail)
}

fn empty_detail(normalized: &str) -> ProjectDetail {
    ProjectDetail {
        meta: Meta::default(),
        name: normalized.to_owned(),
        versions: Vec::new(),
        files: Vec::new(),
    }
}

fn apply_overrides(state: &AppState, hosted: &str, normalized: &str, files: &mut Vec<File>) -> Result<(), SearchError> {
    let overrides: std::collections::HashMap<String, String> =
        state.meta.list_overrides(hosted, normalized)?.into_iter().collect();
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

fn apply_project_status(detail: &mut ProjectDetail) {
    if detail.meta.status() == ProjectStatus::Quarantined {
        detail.files.clear();
    }
}

fn package_source(state: &AppState, index: &Index, normalized: &str) -> Result<PackageSource, SearchError> {
    Ok(match &index.kind {
        IndexKind::Hosted { .. } => PackageSource::Uploaded,
        IndexKind::Cached { .. } => PackageSource::Cached,
        IndexKind::Virtual { upload, .. } => {
            let Some(upload) = upload else {
                return Ok(PackageSource::Cached);
            };
            let upload = state.index_at(*upload);
            if !state.meta.list_upload_entries(&upload.name, normalized)?.is_empty()
                || !state.meta.list_overrides(&upload.name, normalized)?.is_empty()
            {
                PackageSource::Override
            } else {
                PackageSource::Cached
            }
        }
    })
}

fn metadata_doc(state: &AppState, detail: &ProjectDetail) -> Result<Option<CoreMetadataDoc>, SearchError> {
    for file in detail.files.iter().rev() {
        let Some(artifact_sha256) = file.hashes.get("sha256") else {
            continue;
        };
        let Some((_url, metadata_sha256, _source)) = state.meta.get_metadata(artifact_sha256)? else {
            continue;
        };
        let Some(digest) = Digest::from_hex(&metadata_sha256) else {
            continue;
        };
        if !state.blobs.exists(&digest) {
            continue;
        }
        let bytes = state.blobs.read(&digest)?;
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        return Ok(Some(parse_metadata(text)));
    }
    Ok(None)
}

fn search_text(
    display_name: &str,
    normalized: &str,
    detail: &ProjectDetail,
    metadata: Option<&CoreMetadataDoc>,
) -> String {
    let mut text = String::with_capacity(512);
    push_text(&mut text, display_name);
    push_text(&mut text, normalized);
    push_text(&mut text, &detail.name);
    for version in &detail.versions {
        push_text(&mut text, version);
    }
    for file in &detail.files {
        push_text(&mut text, &file.filename);
        if let Some(requires_python) = &file.requires_python {
            push_text(&mut text, requires_python);
        }
    }
    if let Some(metadata) = metadata {
        push_metadata(&mut text, metadata);
    }
    text
}

fn push_metadata(text: &mut String, metadata: &CoreMetadataDoc) {
    for value in [
        metadata.summary.as_deref(),
        metadata.requires_python.as_deref(),
        metadata.license.as_deref(),
        metadata.license_expression.as_deref(),
        metadata.author.as_deref(),
        metadata.maintainer.as_deref(),
        metadata.description_content_type.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        push_text(text, value);
    }
    for values in [
        metadata.keywords.as_slice(),
        metadata.requires_dist.as_slice(),
        metadata.provides_extra.as_slice(),
        metadata.classifiers.as_slice(),
        metadata.license_files.as_slice(),
    ] {
        for value in values {
            push_text(text, value);
        }
    }
    for (label, url) in &metadata.project_urls {
        push_text(text, label);
        push_text(text, url);
    }
    push_text(text, &metadata.description);
}

fn push_text(out: &mut String, value: &str) {
    let value = value.trim();
    if value.is_empty() || out.len() >= INDEXED_TEXT_BYTES {
        return;
    }
    if !out.is_empty() {
        out.push(' ');
    }
    let available = INDEXED_TEXT_BYTES.saturating_sub(out.len());
    out.push_str(truncate_to_chars(value, available));
}

fn project_key<'key>(key: &'key str, index: &str) -> Option<&'key str> {
    let project = key.strip_prefix(index)?.strip_prefix('/')?;
    (!project.is_empty() && !project.contains('/')).then_some(project)
}

fn upload_key<'key>(key: &'key str, index: &str) -> Option<(&'key str, &'key str)> {
    let rest = key.strip_prefix(index)?.strip_prefix('/')?;
    let (project, filename) = rest.split_once('/')?;
    (!project.is_empty() && !filename.is_empty()).then_some((project, filename))
}
