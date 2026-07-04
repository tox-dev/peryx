//! Package search over cached project metadata.

use std::collections::{BTreeSet, HashSet};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use serde::{Deserialize, Serialize};
use tantivy::collector::{Count, TopDocs};
use tantivy::directory::MmapDirectory;
use tantivy::query::{AllQuery, BooleanQuery, Query, RegexQuery, TermQuery};
use tantivy::schema::document::{TantivyDocument, Value as _};
use tantivy::schema::{FAST, Field, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing, TextOptions};
use tantivy::tokenizer::{LowerCaser, NgramTokenizer, TextAnalyzer, TokenizerManager};
use tantivy::{Index as TantivyIndex, IndexReader, Order, Term};
use velodex_ecosystem_pypi::{
    CoreMetadata, CoreMetadataDoc, File, Meta, ProjectDetail, ProjectStatus, parse_detail, parse_metadata,
};
use velodex_storage::blob::Digest;
use velodex_storage::meta::{CachedIndex, MetaScanError};

use crate::path_safety::local_file_url;
use crate::state::{AppState, Index, IndexKind};
use crate::upload::Uploaded;
use velodex_policy::PolicyAction;

const SUBSTRING_TOKENIZER: &str = "velodex_substring";
const MIN_NGRAM: usize = 2;
const MAX_NGRAM: usize = 12;
const INDEXED_TEXT_BYTES: usize = 64 * 1024;
const RAW_REGEX_BYTES: usize = 32 * 1024;
const WRITER_MEMORY_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_PAGE_SIZE: usize = 25;
const PAGE_SIZES: [usize; 3] = [25, 50, 100];
const REGEX_SPECIALS: &str = "\\.+*?()|[]{}^$";

/// Rebuilds a package search index from the current package store.
pub trait PackageIndexer {
    /// Replace the current search index with documents derived from `state`.
    ///
    /// # Errors
    /// Returns a search error when cached package records, blobs, or the derived index cannot be
    /// read or written.
    fn rebuild(&self, state: &AppState) -> Result<(), SearchError>;
}

pub struct PackageSearch {
    index: TantivyIndex,
    reader: IndexReader,
    fields: SearchFields,
    indexed_epoch: Mutex<Option<u64>>,
    rebuild_lock: Mutex<()>,
}

impl PackageSearch {
    /// Build an in-memory package search index.
    ///
    /// # Panics
    /// Panics only if the static schema or tokenizer constants are invalid.
    #[must_use]
    pub fn in_memory() -> Self {
        let (schema, fields) = search_schema();
        Self::from_index(
            TantivyIndex::builder()
                .schema(schema)
                .tokenizers(tokenizers())
                .create_in_ram()
                .expect("search schema and tokenizer constants are valid"),
            fields,
        )
        .expect("in-memory package search reader opens")
    }

    /// Open or create the on-disk package search index.
    ///
    /// # Errors
    /// Returns an error if the directory cannot be created or Tantivy cannot open the index.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SearchError> {
        std::fs::create_dir_all(path.as_ref())?;
        let (schema, fields) = search_schema();
        let index = TantivyIndex::builder()
            .schema(schema)
            .tokenizers(tokenizers())
            .open_or_create(MmapDirectory::open(path.as_ref())?)?;
        Self::from_index(index, fields)
    }

    fn from_index(index: TantivyIndex, fields: SearchFields) -> Result<Self, SearchError> {
        let reader = index
            .reader_builder()
            .reload_policy(tantivy::ReloadPolicy::Manual)
            .try_into()?;
        Ok(Self {
            index,
            reader,
            fields,
            indexed_epoch: Mutex::new(None),
            rebuild_lock: Mutex::new(()),
        })
    }

    /// Search cached package documents.
    ///
    /// # Errors
    /// Returns an error if the derived index cannot refresh or the query is invalid.
    pub fn search(&self, state: &AppState, params: SearchParams) -> Result<SearchResponse, SearchError> {
        self.ensure_current(state)?;
        let query = self.query(&params)?;
        let searcher = self.reader.searcher();
        let offset = params.offset();
        let top_docs = TopDocs::with_limit(params.page_size)
            .and_offset(offset)
            .order_by_string_fast_field("sort", Order::Asc);
        let total = searcher.search(&*query, &Count)?;
        let results = searcher
            .search(&*query, &top_docs)?
            .into_iter()
            .map(|(_sort, address)| {
                searcher
                    .doc::<TantivyDocument>(address)
                    .map(|doc| self.result_from_doc(&doc))
            })
            .collect::<tantivy::Result<Vec<_>>>()?;
        Ok(SearchResponse {
            query: params.query,
            route: params.route,
            source_type: params.source,
            page: params.page,
            page_size: params.page_size,
            total,
            results,
        })
    }

    fn ensure_current(&self, state: &AppState) -> Result<(), SearchError> {
        let epoch = state.epoch.load(Ordering::Relaxed);
        let _guard = self.rebuild_lock.lock().expect("search rebuild lock");
        if self
            .indexed_epoch
            .lock()
            .expect("search epoch lock")
            .is_none_or(|indexed| indexed != epoch)
        {
            self.rebuild(state)?;
            *self.indexed_epoch.lock().expect("search epoch lock") = Some(epoch);
        }
        Ok(())
    }

    fn query(&self, params: &SearchParams) -> Result<Box<dyn Query>, SearchError> {
        let mut queries = vec![self.text_query(params.query.trim())?];
        if let Some(source) = params.source.package_source() {
            queries.push(Box::new(TermQuery::new(
                Term::from_field_text(self.fields.source, source.as_str()),
                IndexRecordOption::Basic,
            )));
        }
        if let Some(route) = &params.route {
            queries.push(Box::new(TermQuery::new(
                Term::from_field_text(self.fields.route, route),
                IndexRecordOption::Basic,
            )));
        }
        Ok(if queries.len() == 1 {
            queries.pop().expect("query exists")
        } else {
            Box::new(BooleanQuery::intersection(queries))
        })
    }

    fn text_query(&self, query: &str) -> Result<Box<dyn Query>, SearchError> {
        if query.is_empty() {
            return Ok(Box::new(AllQuery));
        }
        if let Some(pattern) = query.strip_prefix("re:") {
            if pattern.is_empty() {
                return Ok(Box::new(AllQuery));
            }
            return Ok(Box::new(RegexQuery::from_pattern(
                &format!(".*{}.*", pattern.to_ascii_lowercase()),
                self.fields.raw,
            )?));
        }
        let query = query.to_ascii_lowercase();
        let terms = query_terms(&query);
        if terms.is_empty() {
            let pattern = format!(".*{}.*", escape_regex(&query));
            return Ok(Box::new(RegexQuery::from_pattern(&pattern, self.fields.raw)?));
        }
        let queries = terms
            .into_iter()
            .map(|term| {
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.search, &term),
                    IndexRecordOption::Basic,
                )) as Box<dyn Query>
            })
            .collect();
        Ok(Box::new(BooleanQuery::intersection(queries)))
    }

    fn result_from_doc(&self, doc: &TantivyDocument) -> SearchResult {
        SearchResult {
            display_name: stored_text(doc, self.fields.display),
            normalized_name: stored_text(doc, self.fields.normalized),
            route: stored_text(doc, self.fields.route),
            repository: stored_text(doc, self.fields.repository),
            source_type: PackageSource::from_value(&stored_text(doc, self.fields.source))
                .unwrap_or(PackageSource::Cached),
            summary: non_empty_string(stored_text(doc, self.fields.summary)),
        }
    }

    fn document(&self, package: &PackageDocument) -> TantivyDocument {
        let sort = format!(
            "{}\u{0}{}\u{0}{}",
            package.display_name.to_ascii_lowercase(),
            package.route,
            package.normalized_name
        );
        let mut doc = TantivyDocument::new();
        doc.add_text(self.fields.route, &package.route);
        doc.add_text(self.fields.normalized, &package.normalized_name);
        doc.add_text(self.fields.display, &package.display_name);
        doc.add_text(self.fields.source, package.source.as_str());
        doc.add_text(self.fields.repository, &package.repository);
        doc.add_text(self.fields.summary, package.summary.as_deref().unwrap_or_default());
        doc.add_text(self.fields.sort, sort);
        doc.add_text(self.fields.search, &package.text);
        doc.add_text(
            self.fields.raw,
            truncate_to_chars(&package.text.to_ascii_lowercase(), RAW_REGEX_BYTES),
        );
        doc
    }
}

impl PackageIndexer for PackageSearch {
    fn rebuild(&self, state: &AppState) -> Result<(), SearchError> {
        let mut writer = self
            .index
            .writer_with_num_threads::<TantivyDocument>(1, WRITER_MEMORY_BYTES)?;
        writer.delete_all_documents()?;
        for index in &state.indexes {
            let mut projects = BTreeSet::new();
            collect_projects(state, index, &mut projects)?;
            for normalized in projects {
                let Some(package) = package_document(state, index, &normalized)? else {
                    continue;
                };
                writer.add_document(self.document(&package))?;
            }
        }
        writer.commit()?;
        self.reader.reload()?;
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error(transparent)]
    Tantivy(#[from] tantivy::TantivyError),
    #[error(transparent)]
    Directory(#[from] tantivy::directory::error::OpenDirectoryError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Meta(#[from] velodex_storage::meta::MetaError),
    #[error(transparent)]
    Blob(#[from] velodex_storage::blob::BlobError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Simple(#[from] velodex_ecosystem_pypi::SimpleError),
    #[error("invalid package source type {0:?}")]
    InvalidSource(String),
}

impl From<MetaScanError<Self>> for SearchError {
    fn from(err: MetaScanError<Self>) -> Self {
        match err {
            MetaScanError::Store(err) => Self::Meta(err),
            MetaScanError::Visit(err) => err,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchParams {
    pub query: String,
    pub route: Option<String>,
    pub source: SourceFilter,
    pub page: usize,
    pub page_size: usize,
}

impl Default for SearchParams {
    fn default() -> Self {
        Self {
            query: String::new(),
            route: None,
            source: SourceFilter::All,
            page: 1,
            page_size: DEFAULT_PAGE_SIZE,
        }
    }
}

impl SearchParams {
    /// Parse `/+search` query parameters.
    ///
    /// # Errors
    /// Returns an error for an unknown `type` filter.
    pub fn from_query(query: Option<&str>) -> Result<Self, SearchError> {
        let mut params = Self::default();
        let Some(query) = query else {
            return Ok(params);
        };
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            match key.as_ref() {
                "q" => params.query = value.into_owned(),
                "route" if !value.is_empty() => params.route = Some(value.into_owned()),
                "type" if value.is_empty() || value == "all" => params.source = SourceFilter::All,
                "type" => {
                    params.source = SourceFilter::from_value(&value)
                        .ok_or_else(|| SearchError::InvalidSource(value.into_owned()))?;
                }
                "page" => params.page = value.parse::<usize>().unwrap_or(1).max(1),
                "page_size" => {
                    let page_size = value.parse::<usize>().unwrap_or(DEFAULT_PAGE_SIZE);
                    params.page_size = if PAGE_SIZES.contains(&page_size) {
                        page_size
                    } else {
                        DEFAULT_PAGE_SIZE
                    };
                }
                _ => {}
            }
        }
        Ok(params)
    }

    #[must_use]
    pub const fn offset(&self) -> usize {
        self.page.saturating_sub(1).saturating_mul(self.page_size)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceFilter {
    All,
    Uploaded,
    Cached,
    Override,
}

impl SourceFilter {
    #[must_use]
    pub fn from_value(value: &str) -> Option<Self> {
        match value {
            "all" => Some(Self::All),
            "uploaded" => Some(Self::Uploaded),
            "cached" => Some(Self::Cached),
            "override" => Some(Self::Override),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Uploaded => "uploaded",
            Self::Cached => "cached",
            Self::Override => "override",
        }
    }

    const fn package_source(self) -> Option<PackageSource> {
        match self {
            Self::All => None,
            Self::Uploaded => Some(PackageSource::Uploaded),
            Self::Cached => Some(PackageSource::Cached),
            Self::Override => Some(PackageSource::Override),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PackageSource {
    Uploaded,
    Cached,
    Override,
}

impl PackageSource {
    #[must_use]
    pub fn from_value(value: &str) -> Option<Self> {
        match value {
            "uploaded" => Some(Self::Uploaded),
            "cached" => Some(Self::Cached),
            "override" => Some(Self::Override),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Uploaded => "uploaded",
            Self::Cached => "cached",
            Self::Override => "override",
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Uploaded => "Uploaded",
            Self::Cached => "Cached",
            Self::Override => "Override",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchResponse {
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route: Option<String>,
    #[serde(rename = "type")]
    pub source_type: SourceFilter,
    pub page: usize,
    pub page_size: usize,
    pub total: usize,
    pub results: Vec<SearchResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResult {
    pub display_name: String,
    pub normalized_name: String,
    pub route: String,
    pub repository: String,
    #[serde(rename = "type")]
    pub source_type: PackageSource,
    pub summary: Option<String>,
}

#[derive(Clone, Copy)]
struct SearchFields {
    route: Field,
    normalized: Field,
    display: Field,
    source: Field,
    repository: Field,
    summary: Field,
    sort: Field,
    search: Field,
    raw: Field,
}

struct PackageDocument {
    display_name: String,
    normalized_name: String,
    route: String,
    repository: String,
    source: PackageSource,
    summary: Option<String>,
    text: String,
}

fn search_schema() -> (Schema, SearchFields) {
    let mut builder = Schema::builder();
    let stored = TextOptions::default().set_stored();
    let exact = STRING | STORED;
    let sort = STRING | FAST | STORED;
    let search = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(SUBSTRING_TOKENIZER)
            .set_index_option(IndexRecordOption::Basic)
            .set_fieldnorms(false),
    );
    let raw = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("raw")
            .set_index_option(IndexRecordOption::Basic)
            .set_fieldnorms(false),
    );
    let fields = SearchFields {
        route: builder.add_text_field("route", exact.clone()),
        normalized: builder.add_text_field("normalized", exact.clone()),
        display: builder.add_text_field("display", stored.clone()),
        source: builder.add_text_field("source", exact),
        repository: builder.add_text_field("repository", stored.clone()),
        summary: builder.add_text_field("summary", stored),
        sort: builder.add_text_field("sort", sort),
        search: builder.add_text_field("search", search),
        raw: builder.add_text_field("raw", raw),
    };
    (builder.build(), fields)
}

fn tokenizers() -> TokenizerManager {
    let manager = TokenizerManager::default();
    let tokenizer = TextAnalyzer::builder(
        NgramTokenizer::all_ngrams(MIN_NGRAM, MAX_NGRAM).expect("ngram tokenizer constants are valid"),
    )
    .filter(LowerCaser)
    .build();
    manager.register(SUBSTRING_TOKENIZER, tokenizer);
    manager
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
        repository: index.name.clone(),
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
    let parsed = parse_detail(&record.body)?;
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

fn apply_overrides(state: &AppState, local: &str, normalized: &str, files: &mut Vec<File>) -> Result<(), SearchError> {
    let overrides: std::collections::HashMap<String, String> =
        state.meta.list_overrides(local, normalized)?.into_iter().collect();
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

fn query_terms(query: &str) -> Vec<String> {
    let chars: Vec<char> = query.chars().collect();
    match chars.len() {
        0 | 1 => Vec::new(),
        len if len <= MAX_NGRAM => vec![query.to_owned()],
        len => (0..=len - MAX_NGRAM)
            .map(|start| chars[start..start + MAX_NGRAM].iter().collect::<String>())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
    }
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

fn stored_text(doc: &TantivyDocument, field: Field) -> String {
    doc.get_first(field)
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_owned()
}

fn non_empty_string(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

fn truncate_to_chars(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn escape_regex(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for char in value.chars() {
        if REGEX_SPECIALS.contains(char) {
            escaped.push('\\');
        }
        escaped.push(char);
    }
    escaped
}
