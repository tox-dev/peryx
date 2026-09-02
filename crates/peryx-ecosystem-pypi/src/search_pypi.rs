//! Maps `PyPI` records into neutral search documents.

use std::collections::{BTreeSet, HashSet};
use std::convert::Infallible;

use crate::policy::PypiPolicy;
use crate::source_policy::SourceSelection;
use crate::store::CachedIndex;
use crate::store::PypiStore as _;
use crate::{
    CoreMetadata, CoreMetadataDoc, File, Meta, ProjectDetail, ProjectStatus, Yanked, parse_detail, parse_metadata,
};
use peryx_identity::ArtifactDigest;
use peryx_policy::PolicyAction;
use peryx_storage::blob::Digest;
use peryx_storage::meta::{ArtifactSource, DigestRevocationState, MetaScanError};

use crate::upload::Uploaded;
use peryx_core::path::local_artifact_url;
use peryx_index::{Index, IndexKind};
use peryx_search::{
    ContentSource, INDEXED_TEXT_BYTES, IndexerCtx, ResourceUpdate, SearchDocument, SearchDocumentProvider, SearchError,
    document_key, truncate_to_chars,
};

const IDENTITY_TEXT_BYTES: usize = INDEXED_TEXT_BYTES / 4;
const CORE_METADATA_TEXT_BYTES: usize = INDEXED_TEXT_BYTES / 2;
const CATALOG_TEXT_BYTES: usize = INDEXED_TEXT_BYTES - IDENTITY_TEXT_BYTES - CORE_METADATA_TEXT_BYTES;

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
        IndexKind::Cached { .. } => ctx
            .meta
            .scan_index_records(|key, _value| {
                if let Some(project) = project_record_key(key, &index.name) {
                    projects.insert(project.to_owned());
                }
                Ok::<(), Infallible>(())
            })
            .map_err(infallible_scan_error),
        IndexKind::Hosted { .. } => ctx
            .meta
            .scan_upload_records(|key, _value| {
                if let Some((project, _filename)) = upload_key(key, &index.name) {
                    projects.insert(project.to_owned());
                }
                Ok::<(), Infallible>(())
            })
            .map_err(infallible_scan_error),
        IndexKind::Virtual { layers, .. } => {
            for &position in layers {
                collect_projects(ctx, ctx.index_at(position), projects)?;
            }
            Ok(())
        }
    }
}

fn infallible_scan_error(error: MetaScanError<Infallible>) -> SearchError {
    match error {
        MetaScanError::Store(error) => error.into(),
        MetaScanError::Visit(never) => match never {},
    }
}

pub(crate) fn package_document(
    ctx: &IndexerCtx<'_>,
    index: &Index,
    normalized: &str,
) -> Result<Option<SearchDocument>, SearchError> {
    let mut detail = index_detail(ctx, index, normalized, &index.route)?;
    drop_revoked_files(ctx, &mut detail)?;
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

/// Removes every file the served page hides, mirroring `cache::resolve::filter_revoked_files`, so
/// emptiness, local availability, and the indexed text all describe only files an installer can still
/// fetch. Leaving the project's version list alone keeps the document in step with the page, which also
/// reports the versions upstream declares rather than the ones its surviving files carry.
///
/// The transactional active count answers a repository with no revocation in one read, so the common
/// crawl pays a single lookup per project rather than one per file.
fn drop_revoked_files(ctx: &IndexerCtx<'_>, detail: &mut ProjectDetail) -> Result<(), SearchError> {
    if !ctx.meta.has_active_digest_revocation()? {
        return Ok(());
    }
    let mut kept = Vec::with_capacity(detail.files.len());
    for file in std::mem::take(&mut detail.files) {
        if !is_revoked(ctx, &file)? {
            kept.push(file);
        }
    }
    detail.files = kept;
    Ok(())
}

/// A file without a usable SHA-256 has nothing a digest revocation can name, so it survives; the byte
/// routes cannot serve it under a revocation either, since they resolve the same digest first.
fn is_revoked(ctx: &IndexerCtx<'_>, file: &File) -> Result<bool, SearchError> {
    let Some(digest) = file
        .hashes
        .get("sha256")
        .and_then(|sha256| ArtifactDigest::from_sha256(sha256).ok())
    else {
        return Ok(false);
    };
    Ok(ctx
        .meta
        .digest_revocation(&digest)?
        .is_some_and(|record| matches!(record.state, DigestRevocationState::Active)))
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

/// The document view of one index: a merge for a virtual repository, the stored records otherwise.
fn index_detail(
    ctx: &IndexerCtx<'_>,
    index: &Index,
    normalized: &str,
    serve_route: &str,
) -> Result<ProjectDetail, SearchError> {
    let IndexKind::Virtual { layers, write_target } = &index.kind else {
        return leaf_detail(ctx, index, normalized, serve_route);
    };
    let selection = SourceSelection::new(index, normalized);
    let candidates = virtual_candidates(ctx, &selection, layers, *write_target, normalized, serve_route)?;
    Ok(apply_index_policy(
        index,
        normalized,
        merge_candidates(normalized, candidates),
    ))
}

/// One leaf index's stored records for the project, with that index's own policy applied: a hosted
/// index's uploads, or the page a mirror already fetched.
fn leaf_detail(
    ctx: &IndexerCtx<'_>,
    index: &Index,
    normalized: &str,
    serve_route: &str,
) -> Result<ProjectDetail, SearchError> {
    let detail = if matches!(index.kind, IndexKind::Hosted { .. }) {
        local_detail(ctx, &index.name, normalized, serve_route)?
    } else {
        mirror_detail(ctx, index, normalized, serve_route)?
    };
    Ok(apply_index_policy(index, normalized, detail))
}

fn apply_index_policy(index: &Index, normalized: &str, detail: ProjectDetail) -> ProjectDetail {
    index
        .policy
        .apply_detail(PolicyAction::Serve, normalized, detail, None)
        .unwrap_or_else(|_| empty_detail(normalized))
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

/// The leaf candidates a virtual index contributes to the search document, each paired with the leaf
/// that produced it and ranked hosted-before-cached, so an indexed project describes the hosted file
/// that shadows upstream rather than the file it shadows. The served page ranks the same leaves.
///
/// `selection` decides which of them contribute, so a document never advertises a name, version, or
/// summary the route's source policy withholds from the served page, and an enclosing repository's
/// refusal of cached content travels down with it the way resolution carries it. Resolution logs the
/// private-first collision when it serves one; the indexer would repeat that entry on every crawl.
fn virtual_candidates(
    ctx: &IndexerCtx<'_>,
    selection: &SourceSelection,
    layers: &[usize],
    upload: Option<usize>,
    normalized: &str,
    serve_route: &str,
) -> Result<Vec<(usize, ProjectDetail)>, SearchError> {
    let consult_cached = selection.consults_cached();
    let mut candidates = Vec::new();
    for position in selection.members(ctx.indexes, layers) {
        let found = member_candidates(ctx, position, normalized, serve_route, consult_cached)?;
        candidates.extend(found);
    }
    selection.select(ctx.indexes, &mut candidates, |detail| !detail.files.is_empty());
    if let Some(position) = upload {
        apply_overrides(ctx, &ctx.index_at(position).name, normalized, &mut candidates)?;
    }
    Ok(candidates)
}

/// The leaf candidates one member offers, with that member's own policy already applied.
fn member_candidates(
    ctx: &IndexerCtx<'_>,
    position: usize,
    normalized: &str,
    serve_route: &str,
    consult_cached: bool,
) -> Result<Vec<(usize, ProjectDetail)>, SearchError> {
    let member = ctx.index_at(position);
    let IndexKind::Virtual { layers, write_target } = &member.kind else {
        return Ok(vec![(position, leaf_detail(ctx, member, normalized, serve_route)?)]);
    };
    let selection = SourceSelection::new(member, normalized).under_cached_refusal(!consult_cached);
    let candidates = virtual_candidates(ctx, &selection, layers, *write_target, normalized, serve_route)?;
    Ok(candidates
        .into_iter()
        .map(|(leaf, detail)| (leaf, apply_index_policy(member, normalized, detail)))
        .collect())
}

/// Merge ranked candidates into the document view, keeping the first candidate to claim a filename.
fn merge_candidates(normalized: &str, candidates: Vec<(usize, ProjectDetail)>) -> ProjectDetail {
    let mut files = Vec::new();
    let mut seen = BTreeSet::new();
    let mut versions = BTreeSet::new();
    let mut meta = Meta::default();
    for (_, detail) in candidates {
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
    let mut detail = ProjectDetail {
        meta,
        name: normalized.to_owned(),
        versions: versions.into_iter().collect(),
        files,
    };
    apply_project_status(&mut detail);
    detail
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
    candidates: &mut [(usize, ProjectDetail)],
) -> Result<(), SearchError> {
    let overrides = ctx.meta.list_overrides(hosted, normalized)?;
    if overrides.is_empty() {
        return Ok(());
    }
    for (_, detail) in candidates {
        detail
            .files
            .retain(|file| !overrides.get(&file.filename).is_some_and(|record| record.hidden));
        for file in &mut detail.files {
            if let Some(record) = overrides.get(&file.filename)
                && record.yanked != Yanked::No
            {
                file.yanked = record.yanked.clone();
            }
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
        let Some(metadata_sha256) = ctx.meta.get_metadata_digest(artifact_sha256)? else {
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
    let mut identity = String::with_capacity(512);
    push_unique_text(
        &mut identity,
        [normalized, detail.name.as_str(), display_label],
        IDENTITY_TEXT_BYTES,
    );

    let mut core_metadata = String::with_capacity(512);
    if let Some(metadata) = metadata {
        push_metadata(&mut core_metadata, metadata, CORE_METADATA_TEXT_BYTES);
    }

    let catalog_limit = CATALOG_TEXT_BYTES
        + IDENTITY_TEXT_BYTES.saturating_sub(identity.len())
        + CORE_METADATA_TEXT_BYTES.saturating_sub(core_metadata.len());
    let mut catalog = String::with_capacity(512);
    push_unique_text(
        &mut catalog,
        detail
            .versions
            .iter()
            .map(String::as_str)
            .chain(detail.files.iter().flat_map(|file| {
                [Some(file.filename.as_str()), file.requires_python.as_deref()]
                    .into_iter()
                    .flatten()
            })),
        catalog_limit,
    );

    let mut text = String::with_capacity(512);
    for section in [&identity, &core_metadata, &catalog] {
        push_text(&mut text, section, INDEXED_TEXT_BYTES);
    }
    text
}

fn push_metadata(text: &mut String, metadata: &CoreMetadataDoc, limit: usize) {
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
        push_text(text, value, limit);
    }
    for values in [
        metadata.keywords.as_slice(),
        metadata.requires_dist.as_slice(),
        metadata.provides_extra.as_slice(),
        metadata.classifiers.as_slice(),
        metadata.license_files.as_slice(),
    ] {
        for value in values {
            push_text(text, value, limit);
        }
    }
    for value in metadata.import_names.iter().chain(&metadata.import_namespaces) {
        push_text(text, crate::metadata::import_parts(value).0, limit);
    }
    for (label, url) in &metadata.project_urls {
        push_text(text, label, limit);
        push_text(text, url, limit);
    }
    if let Some(home_page) = &metadata.home_page {
        push_text(text, home_page, limit);
    }
    push_text(text, &metadata.description, limit);
}

fn push_unique_text<'a>(out: &mut String, values: impl IntoIterator<Item = &'a str>, limit: usize) {
    let mut seen = HashSet::new();
    for value in values {
        if out.len() >= limit {
            break;
        }
        let value = value.trim();
        if seen.insert(value) {
            push_text(out, value, limit);
        }
    }
}

fn push_text(out: &mut String, value: &str, limit: usize) {
    let value = value.trim();
    if value.is_empty() || out.len() >= limit {
        return;
    }
    if !out.is_empty() {
        out.push(' ');
    }
    let available = limit.saturating_sub(out.len());
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
