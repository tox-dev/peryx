//! Package search over cached project metadata.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tantivy::collector::{Count, TopDocs};
use tantivy::directory::MmapDirectory;
use tantivy::query::{AllQuery, BooleanQuery, Query, RegexQuery, TermQuery};
use tantivy::schema::document::{TantivyDocument, Value as _};
use tantivy::schema::{FAST, Field, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing, TextOptions};
use tantivy::tokenizer::{LowerCaser, NgramTokenizer, TextAnalyzer, TokenizerManager};
use tantivy::{Index as TantivyIndex, IndexReader, Order, Term};
use velodex_storage::meta::MetaScanError;

use crate::state::AppState;

const SUBSTRING_TOKENIZER: &str = "velodex_substring";
const MIN_NGRAM: usize = 2;
const MAX_NGRAM: usize = 12;
pub const INDEXED_TEXT_BYTES: usize = 64 * 1024;
const RAW_REGEX_BYTES: usize = 32 * 1024;
const WRITER_MEMORY_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_PAGE_SIZE: usize = 25;
const PAGE_SIZES: [usize; 3] = [25, 50, 100];
const REGEX_SPECIALS: &str = "\\.+*?()|[]{}^$";

/// Produces the search documents for one ecosystem's stored packages.
///
/// The tantivy index, schema, and querying are ecosystem-neutral; only turning an index's stored
/// records into searchable [`PackageDocument`]s is format-specific, so it sits behind this seam. The
/// binary injects the configured ecosystem's indexer; a build with none wired in gets
/// [`EmptyIndexer`].
pub trait PackageIndexer: Send + Sync {
    /// Every searchable document derivable from `state`, replacing the current index contents.
    ///
    /// # Errors
    /// Returns a search error when cached package records or blobs cannot be read.
    fn documents(&self, state: &AppState) -> Result<Vec<PackageDocument>, SearchError>;
}

/// The indexer installed until an ecosystem driver is wired into the state: it yields no documents.
///
/// A freshly built [`AppState`] searches an empty index until [`AppState::set_ecosystem`] injects the
/// real indexer, which keeps the neutral crate free of any format-specific indexing logic.
#[derive(Debug, Clone, Copy, Default)]
pub struct EmptyIndexer;

impl PackageIndexer for EmptyIndexer {
    fn documents(&self, _state: &AppState) -> Result<Vec<PackageDocument>, SearchError> {
        Ok(Vec::new())
    }
}

fn default_indexer() -> Arc<dyn PackageIndexer> {
    Arc::new(EmptyIndexer)
}

pub struct PackageSearch {
    index: TantivyIndex,
    reader: IndexReader,
    fields: SearchFields,
    indexer: Arc<dyn PackageIndexer>,
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
            indexer: default_indexer(),
            indexed_epoch: Mutex::new(None),
            rebuild_lock: Mutex::new(()),
        })
    }

    /// Replace the ecosystem indexer. The binary injects the configured ecosystem's indexer at
    /// startup; without this the search index stays empty (see [`EmptyIndexer`]).
    pub fn set_indexer(&mut self, indexer: Arc<dyn PackageIndexer>) {
        self.indexer = indexer;
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
            self.write(&self.indexer.documents(state)?)?;
            *self.indexed_epoch.lock().expect("search epoch lock") = Some(epoch);
        }
        Ok(())
    }

    /// Replace the whole index with `documents`, then make them searchable.
    fn write(&self, documents: &[PackageDocument]) -> Result<(), SearchError> {
        let mut writer = self
            .index
            .writer_with_num_threads::<TantivyDocument>(1, WRITER_MEMORY_BYTES)?;
        writer.delete_all_documents()?;
        for package in documents {
            writer.add_document(self.document(package))?;
        }
        writer.commit()?;
        self.reader.reload()?;
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
            index: stored_text(doc, self.fields.index),
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
        doc.add_text(self.fields.index, &package.index);
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
    /// The ecosystem indexer failed to derive a document from a stored record.
    #[error("indexing failed: {0}")]
    Indexer(String),
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
    pub index: String,
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
    index: Field,
    summary: Field,
    sort: Field,
    search: Field,
    raw: Field,
}

/// One searchable package, produced by a [`PackageIndexer`] and stored in the tantivy index. The
/// fields are ecosystem-neutral; the indexer decides how to fill them from its format's records.
pub struct PackageDocument {
    pub display_name: String,
    pub normalized_name: String,
    pub route: String,
    pub index: String,
    pub source: PackageSource,
    pub summary: Option<String>,
    pub text: String,
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
        index: builder.add_text_field("index", stored.clone()),
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

fn stored_text(doc: &TantivyDocument, field: Field) -> String {
    doc.get_first(field)
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_owned()
}

fn non_empty_string(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

#[must_use]
pub fn truncate_to_chars(value: &str, max_bytes: usize) -> &str {
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
