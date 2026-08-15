//! Maps `PyPI` records into neutral search documents.

use std::collections::{BTreeMap, BTreeSet};

use crate::policy::PypiPolicy;
use crate::store::CachedIndex;
use crate::store::PypiStore as _;
use crate::{CoreMetadata, CoreMetadataDoc, File, Meta, ProjectDetail, ProjectStatus, parse_detail, parse_metadata};
use peryx_policy::PolicyAction;
use peryx_storage::blob::Digest;
use peryx_storage::meta::ArtifactSource;

use crate::upload::Uploaded;
use peryx_core::path::local_artifact_url;
use peryx_index::{Index, IndexKind};
use peryx_search::{
    ContentSource, INDEXED_TEXT_BYTES, IndexerCtx, ResourceUpdate, SearchDocument, SearchDocumentProvider, SearchError,
    document_key, truncate_to_chars,
};

/// Produces `PyPI` search documents for the neutral search index.
#[derive(Debug, Clone, Copy, Default)]
pub struct PypiIndexer;

impl SearchDocumentProvider for PypiIndexer {
    fn documents(&self, ctx: &IndexerCtx<'_>) -> Result<Vec<SearchDocument>, SearchError> {
        let mut documents = Vec::new();
        for index in ctx.indexes {
            let mut projects = BTreeSet::new();
            collect_projects(ctx, index, &mut projects)?;
            for normalized in projects {
                if let Some(package) = package_document(ctx, index, &normalized)? {
                    documents.push(package);
                }
            }
        }
        Ok(documents)
    }

    /// Re-derive one project across every index it can appear on, mirroring [`documents`] for a single
    /// name. Each index contributes the project's key so a deletion retires the stale document there, plus
    /// the freshly derived document when the project still has files, so the neutral engine rewrites only
    /// this project. A non-PyPI index holds no record for the name and so derives none, exactly as the
    /// full walk finds nothing there.
    ///
    /// [`documents`]: PypiIndexer::documents
    fn resource_update(&self, ctx: &IndexerCtx<'_>, name: &str) -> Result<ResourceUpdate, SearchError> {
        let mut update = ResourceUpdate::default();
        for index in ctx.indexes {
            update.keys.push(document_key(&index.route, name));
            if let Some(package) = package_document(ctx, index, name)? {
                update.documents.push(package);
            }
        }
        Ok(update)
    }
}

fn collect_projects(ctx: &IndexerCtx<'_>, index: &Index, projects: &mut BTreeSet<String>) -> Result<(), SearchError> {
    match &index.kind {
        IndexKind::Cached { .. } => {
            ctx.meta.scan_index_records(|key, _value| {
                if let Some(project) = project_record_key(key, &index.name) {
                    projects.insert(project.to_owned());
                }
                Ok(())
            })?;
        }
        IndexKind::Hosted { .. } => {
            ctx.meta.scan_upload_records(|key, _value| {
                if let Some((project, _filename)) = upload_key(key, &index.name) {
                    projects.insert(project.to_owned());
                }
                Ok(())
            })?;
        }
        IndexKind::Virtual { layers, .. } => {
            for &position in layers {
                collect_projects(ctx, ctx.index_at(position), projects)?;
            }
        }
    }
    Ok(())
}

pub(crate) fn package_document(
    ctx: &IndexerCtx<'_>,
    index: &Index,
    normalized: &str,
) -> Result<Option<SearchDocument>, SearchError> {
    let detail = cached_detail(ctx, index, normalized, &index.route)?;
    if detail.files.is_empty() {
        return Ok(None);
    }
    let source = content_source(ctx, index, normalized)?;
    let available_locally = available_locally(ctx, index, normalized, &detail)?;
    let metadata = metadata_doc(ctx, &detail)?;
    let display_label = metadata
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
    Ok(Some(SearchDocument {
        text: search_text(&display_label, normalized, &detail, metadata.as_ref()),
        display_label,
        resource_key: normalized.to_owned(),
        route: index.route.clone(),
        index: index.name.clone(),
        ecosystem: index.ecosystem.as_str().to_owned(),
        source,
        available_locally,
        summary,
    }))
}

/// Whether any of the project's distributions can be served from local storage right now, decided
/// from the #441 placement projection so search agrees with the file view [`apply_placement`] renders
/// without a per-result content-store probe.
///
/// A hosted upload's bytes are local unless a hosted-source placement marks them evicted; the upload
/// path records no placement for a still-present file, so hosted-layer membership stands in for one. A
/// mirrored file is local only when its placement projects [`ByteAvailability::Local`]: a never-fetched
/// upstream catalog entry has none and stays remote.
///
/// [`apply_placement`]: crate::serving::web
fn available_locally(
    ctx: &IndexerCtx<'_>,
    index: &Index,
    normalized: &str,
    detail: &ProjectDetail,
) -> Result<bool, SearchError> {
    let mut hosted = BTreeSet::new();
    collect_hosted_filenames(ctx, index, normalized, &mut hosted)?;
    for file in &detail.files {
        let Some(sha256) = file.hashes.get("sha256") else {
            continue;
        };
        let placement = ctx.meta.get_artifact_placement(sha256)?;
        let local = if hosted.contains(&file.filename) {
            // A hosted upload's bytes are local unless its own hosted-source placement marks them
            // gone; a stale proxied row left by a same-digest mirror never overrides the upload.
            match placement {
                Some(placement) if placement.source == ArtifactSource::Hosted => placement.availability.is_local(),
                _ => true,
            }
        } else {
            placement.is_some_and(|placement| placement.availability.is_local())
        };
        if local {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The filenames the index's hosted (upload) layers published for `normalized`, unioned across a
/// virtual index's layers, mirroring the serving path so a merged project agrees on which files are
/// uploaded rather than mirrored.
fn collect_hosted_filenames(
    ctx: &IndexerCtx<'_>,
    index: &Index,
    normalized: &str,
    names: &mut BTreeSet<String>,
) -> Result<(), SearchError> {
    match &index.kind {
        IndexKind::Hosted { .. } => {
            for (filename, _record) in ctx.meta.list_upload_entries(&index.name, normalized)? {
                names.insert(filename);
            }
        }
        IndexKind::Virtual { layers, .. } => {
            for &position in layers {
                collect_hosted_filenames(ctx, ctx.index_at(position), normalized, names)?;
            }
        }
        IndexKind::Cached { .. } => {}
    }
    Ok(())
}

fn cached_detail(
    ctx: &IndexerCtx<'_>,
    index: &Index,
    normalized: &str,
    serve_route: &str,
) -> Result<ProjectDetail, SearchError> {
    let detail = match &index.kind {
        IndexKind::Cached { .. } => mirror_detail(ctx, index, normalized, serve_route),
        IndexKind::Hosted { .. } => local_detail(ctx, &index.name, normalized, serve_route),
        IndexKind::Virtual { layers, write_target } => {
            virtual_detail(ctx, layers, *write_target, normalized, serve_route)
        }
    }?;
    Ok(index
        .policy
        .apply_detail(PolicyAction::Serve, normalized, detail, None)
        .unwrap_or_else(|_| empty_detail(normalized)))
}

fn mirror_detail(
    ctx: &IndexerCtx<'_>,
    index: &Index,
    normalized: &str,
    serve_route: &str,
) -> Result<ProjectDetail, SearchError> {
    let Some(record) = ctx.meta.get_index(&format!("{}/{normalized}", index.name))? else {
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
        file.url = local_artifact_url(route, &sha256, &file.filename);
    }
    file
}

fn local_detail(
    ctx: &IndexerCtx<'_>,
    name: &str,
    normalized: &str,
    serve_route: &str,
) -> Result<ProjectDetail, SearchError> {
    let entries = ctx.meta.list_upload_entries(name, normalized)?;
    if entries.is_empty() {
        return Ok(empty_detail(normalized));
    }
    let mut files = Vec::with_capacity(entries.len());
    let mut versions = BTreeSet::new();
    for (_filename, bytes) in entries {
        let mut uploaded: Uploaded = serde_json::from_slice(&bytes)?;
        // A soft-deleted upload is hidden from serving (see cache::resolve::local_detail); keep search in
        // step so a trashed file never outlives package serving in the index.
        if uploaded.trashed.is_some() {
            continue;
        }
        versions.insert(uploaded.version);
        if let Some(sha256) = uploaded.file.hashes.get("sha256") {
            uploaded.file.url = local_artifact_url(serve_route, sha256, &uploaded.file.filename);
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

/// Merge a virtual index's layers for the search document, resolving cached layers last so an
/// indexed project describes the hosted file that shadows upstream rather than the file it shadows.
/// The served page merges by the same precedence.
fn virtual_detail(
    ctx: &IndexerCtx<'_>,
    layers: &[usize],
    upload: Option<usize>,
    normalized: &str,
    serve_route: &str,
) -> Result<ProjectDetail, SearchError> {
    let mut files = Vec::new();
    let mut seen = BTreeSet::new();
    let mut versions = BTreeSet::new();
    let mut meta = Meta::default();
    for position in peryx_index::shadow_order(ctx.indexes, layers) {
        let detail = cached_detail(ctx, ctx.index_at(position), normalized, serve_route)?;
        // Merge status before skipping empty members; quarantine must dominate member order.
        if detail.meta.status().severity() > meta.status().severity() {
            meta.project_status = detail.meta.project_status;
            meta.project_status_reason = detail.meta.project_status_reason;
        }
        if detail.files.is_empty() {
            continue;
        }
        versions.extend(detail.versions);
        for file in detail.files {
            if seen.insert(file.filename.clone()) {
                files.push(file);
            }
        }
    }
    if let Some(position) = upload {
        apply_overrides(ctx, &ctx.index_at(position).name, normalized, &mut files)?;
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

fn apply_overrides(
    ctx: &IndexerCtx<'_>,
    hosted: &str,
    normalized: &str,
    files: &mut Vec<File>,
) -> Result<(), SearchError> {
    let overrides: BTreeMap<String, String> = ctx.meta.list_overrides(hosted, normalized)?.into_iter().collect();
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

fn content_source(ctx: &IndexerCtx<'_>, index: &Index, normalized: &str) -> Result<ContentSource, SearchError> {
    Ok(match &index.kind {
        IndexKind::Hosted { .. } => ContentSource::Uploaded,
        IndexKind::Cached { .. } => ContentSource::Cached,
        IndexKind::Virtual { write_target, .. } => {
            let Some(write_target) = write_target else {
                return Ok(ContentSource::Cached);
            };
            let upload = ctx.index_at(*write_target);
            if !ctx.meta.list_upload_entries(&upload.name, normalized)?.is_empty()
                || !ctx.meta.list_overrides(&upload.name, normalized)?.is_empty()
            {
                ContentSource::Override
            } else {
                ContentSource::Cached
            }
        }
    })
}

fn metadata_doc(ctx: &IndexerCtx<'_>, detail: &ProjectDetail) -> Result<Option<CoreMetadataDoc>, SearchError> {
    for file in detail.files.iter().rev() {
        let Some(artifact_sha256) = file.hashes.get("sha256") else {
            continue;
        };
        let Some((_url, metadata_sha256, _source)) = ctx.meta.get_metadata(artifact_sha256)? else {
            continue;
        };
        let Some(digest) = Digest::from_hex(&metadata_sha256) else {
            continue;
        };
        if ctx.blobs.blocking().head(&digest)?.is_none() {
            continue;
        }
        let bytes = ctx
            .blobs
            .blocking()
            .read_bytes(&digest, crate::archive::MAX_WHEEL_METADATA_BYTES)?;
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        // A cached upstream sibling can be malformed; index the next file rather than fail the crawl.
        let Ok(doc) = parse_metadata(text) else {
            continue;
        };
        return Ok(Some(doc));
    }
    Ok(None)
}

fn search_text(
    display_label: &str,
    normalized: &str,
    detail: &ProjectDetail,
    metadata: Option<&CoreMetadataDoc>,
) -> String {
    let mut text = String::with_capacity(512);
    push_text(&mut text, display_label);
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
        metadata.author_email.as_deref(),
        metadata.maintainer.as_deref(),
        metadata.maintainer_email.as_deref(),
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
    for value in metadata.import_names.iter().chain(&metadata.import_namespaces) {
        push_text(text, crate::metadata::import_parts(value).0);
    }
    for (label, url) in &metadata.project_urls {
        push_text(text, label);
        push_text(text, url);
    }
    if let Some(home_page) = &metadata.home_page {
        push_text(text, home_page);
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

fn project_record_key<'key>(key: &'key str, index: &str) -> Option<&'key str> {
    let project = key.strip_prefix(index)?.strip_prefix('/')?;
    (!project.is_empty() && !project.contains('/')).then_some(project)
}

fn upload_key<'key>(key: &'key str, index: &str) -> Option<(&'key str, &'key str)> {
    let rest = key.strip_prefix(index)?.strip_prefix('/')?;
    let (project, filename) = rest.split_once('/')?;
    (!project.is_empty() && !filename.is_empty()).then_some((project, filename))
}
