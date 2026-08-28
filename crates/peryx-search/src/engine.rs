use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroUsize;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tantivy::collector::{Count, TopDocs};
use tantivy::directory::MmapDirectory;
use tantivy::query::{AllQuery, BooleanQuery, EmptyQuery, Query, RegexQuery, TermQuery};
use tantivy::schema::document::{TantivyDocument, Value as _};
use tantivy::schema::{FAST, Field, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing, TextOptions};
use tantivy::tokenizer::{LowerCaser, NgramTokenizer, TextAnalyzer, TokenizerManager};
use tantivy::{Index as TantivyIndex, IndexReader, Order, Term};

use crate::SEARCH_VIEW;
use crate::access::{SearchAccess, SearchAccessPattern};
use crate::context::{IndexerCtx, SearchCtx};
use crate::error::SearchError;
use crate::indexer::{CompositeIndexer, SearchDocument, SearchDocumentProvider, default_indexer};
use crate::params::{ContentSource, SearchParams};
use crate::response::{SearchResponse, SearchResult};

const SUBSTRING_TOKENIZER: &str = "peryx_substring";
const MIN_NGRAM: usize = 2;
const MAX_NGRAM: usize = 12;
const RAW_REGEX_BYTES: usize = 32 * 1024;
const WRITER_MEMORY_BYTES: usize = 64 * 1024 * 1024;
const REGEX_SPECIALS: &str = "\\.+*?()|[]{}^$";
const AVAILABLE_LOCAL: &str = "local";
const AVAILABLE_REMOTE: &str = "remote";

pub struct SearchIndex {
    index: TantivyIndex,
    reader: IndexReader,
    fields: SearchFields,
    indexer: Arc<dyn SearchDocumentProvider>,
    state: Mutex<IndexState>,
    rebuild_lock: Mutex<()>,
    home: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RebuildProgress {
    pub indexed: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebuildOutcome {
    Published {
        documents: u64,
    },
    /// Cancellation keeps the previously published index.
    Aborted {
        documents: u64,
    },
}

impl SearchIndex {
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
            None,
        )
        .expect("in-memory resource search reader opens")
    }

    /// Schema mismatches discard the derived index instead of blocking startup.
    ///
    /// # Errors
    /// Returns an error if the directory cannot be created or read, or Tantivy cannot open the index
    /// for a reason other than a schema change.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SearchError> {
        Self::open_path(path.as_ref())
    }

    fn open_path(path: &Path) -> Result<Self, SearchError> {
        std::fs::create_dir_all(path)?;
        if rebuild_marker(path).exists() {
            tracing::warn!(path = %path.display(), "search index rebuild was interrupted; discarding the partial index");
            reset_dir(path)?;
            std::fs::remove_file(rebuild_marker(path))?;
        }
        let (schema, fields) = search_schema();
        let index = match open_index(path, &schema) {
            Err(SearchError::Tantivy(tantivy::TantivyError::SchemaError(_))) => {
                tracing::warn!(path = %path.display(), "search index schema changed; rebuilding it");
                reset_dir(path)?;
                open_index(path, &schema)?
            }
            result => result?,
        };
        Self::from_index(index, fields, Some(path.to_path_buf()))
    }

    fn from_index(index: TantivyIndex, fields: SearchFields, home: Option<PathBuf>) -> Result<Self, SearchError> {
        let reader = index
            .reader_builder()
            .reload_policy(tantivy::ReloadPolicy::Manual)
            .try_into()?;
        Ok(Self {
            index,
            reader,
            fields,
            indexer: default_indexer(),
            state: Mutex::new(IndexState::default()),
            rebuild_lock: Mutex::new(()),
            home,
        })
    }

    pub fn add_indexer(&mut self, indexer: Arc<dyn SearchDocumentProvider>) {
        let current = std::mem::replace(&mut self.indexer, default_indexer());
        self.indexer = Arc::new(CompositeIndexer(vec![current, indexer]));
    }

    /// # Panics
    /// Panics if the search-state lock was poisoned by a prior panic while holding it.
    pub fn bump_epoch(&self) {
        let mut state = self.state.lock().expect("search state lock");
        state.epoch += 1;
        state.dirty = None;
    }

    /// Does not narrow a pending full rebuild.
    ///
    /// # Panics
    /// Panics if the search-state lock was poisoned by a prior panic while holding it.
    pub fn invalidate_resource(&self, name: &str) {
        let mut state = self.state.lock().expect("search state lock");
        state.epoch += 1;
        let epoch = state.epoch;
        if let Some(dirty) = state.dirty.as_mut() {
            dirty.insert(name.to_owned(), epoch);
        }
    }

    /// Derivation runs outside the writer lock. A changed store serial forces one locked re-derivation
    /// so a concurrent scoped update cannot be overwritten. Cancellation preserves the published index.
    ///
    /// # Errors
    /// Returns a search error if derivation, publication, or in-flight marker maintenance fails.
    ///
    /// # Panics
    /// Panics if the rebuild lock was poisoned by a prior panic while rebuilding.
    pub fn rebuild(
        &self,
        ctx: &IndexerCtx<'_>,
        chunk: NonZeroUsize,
        observe: &mut dyn FnMut(RebuildProgress) -> ControlFlow<()>,
    ) -> Result<RebuildOutcome, SearchError> {
        let mut snapshot = self.snapshot(ctx)?;
        let _guard = self.rebuild_lock.lock().expect("search rebuild lock");
        if ctx.meta.current_serial()? != snapshot.frontier {
            snapshot = self.snapshot(ctx)?;
        }
        let total = snapshot.documents.len() as u64;
        self.mark_rebuilding()?;
        let mut writer = self
            .index
            .writer_with_num_threads::<TantivyDocument>(1, WRITER_MEMORY_BYTES)?;
        writer.delete_all_documents()?;
        let mut indexed = 0_u64;
        for slice in snapshot.documents.chunks(chunk.get()) {
            if observe(RebuildProgress { indexed, total }).is_break() {
                writer.rollback()?;
                self.clear_rebuilding()?;
                return Ok(RebuildOutcome::Aborted { documents: indexed });
            }
            for resource in slice {
                writer.add_document(self.document(resource))?;
            }
            indexed += slice.len() as u64;
        }
        let _ = observe(RebuildProgress { indexed, total });
        writer.commit()?;
        self.reader.reload()?;
        self.clear_rebuilding()?;
        let mut state = self.state.lock().expect("search state lock");
        if state.epoch == snapshot.epoch {
            state.dirty = Some(BTreeMap::new());
        }
        state.indexed_epoch = Some(snapshot.epoch);
        drop(state);
        ctx.meta.set_view_frontier(SEARCH_VIEW, snapshot.frontier)?;
        Ok(RebuildOutcome::Published { documents: indexed })
    }

    fn snapshot(&self, ctx: &IndexerCtx<'_>) -> Result<RebuildSnapshot, SearchError> {
        let epoch = self.state.lock().expect("search state lock").epoch;
        let frontier = ctx.meta.current_serial()?;
        let documents = self.indexer.documents(ctx)?;
        Ok(RebuildSnapshot {
            epoch,
            frontier,
            documents,
        })
    }

    fn mark_rebuilding(&self) -> Result<(), SearchError> {
        if let Some(home) = &self.home {
            std::fs::write(rebuild_marker(home), [])?;
        }
        Ok(())
    }

    fn clear_rebuilding(&self) -> Result<(), SearchError> {
        if let Some(home) = &self.home {
            std::fs::remove_file(rebuild_marker(home))?;
        }
        Ok(())
    }

    /// # Errors
    /// Returns an error if the derived index cannot refresh or the query is invalid.
    pub fn search(&self, ctx: &SearchCtx<'_>, params: SearchParams) -> Result<SearchResponse, SearchError> {
        self.search_with_access(ctx, params, None)
    }

    /// Apply access before collection so totals cannot reveal unreadable resources.
    ///
    /// # Errors
    /// Returns an error if the derived index cannot refresh or the query is invalid.
    pub fn search_authorized(
        &self,
        ctx: &SearchCtx<'_>,
        params: SearchParams,
        access: &SearchAccess,
    ) -> Result<SearchResponse, SearchError> {
        self.search_with_access(ctx, params, Some(access))
    }

    fn search_with_access(
        &self,
        ctx: &SearchCtx<'_>,
        params: SearchParams,
        access: Option<&SearchAccess>,
    ) -> Result<SearchResponse, SearchError> {
        self.ensure_current(ctx)?;
        let query = self.query(&params, access)?;
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
                let mut result = self.result_from_doc(&searcher.doc::<TantivyDocument>(address)?);
                let ecosystem = result
                    .ecosystem
                    .parse()
                    .map_err(|_| SearchError::InvalidEcosystem(result.ecosystem.clone()))?;
                ctx.lexicon(&ecosystem).resource_kind.clone_into(&mut result.type_label);
                Ok(result)
            })
            .collect::<Result<Vec<_>, SearchError>>()?;
        Ok(SearchResponse {
            query: params.query,
            route: params.route,
            source_type: params.source,
            availability: params.availability,
            page: params.page,
            page_size: params.page_size,
            total,
            results,
        })
    }

    fn ensure_current(&self, ctx: &SearchCtx<'_>) -> Result<(), SearchError> {
        // The published reader stays complete while another writer holds the rebuild lock.
        let Ok(_guard) = self.rebuild_lock.try_lock() else {
            return Ok(());
        };
        let (epoch, indexed, scoped) = {
            let state = self.state.lock().expect("search state lock");
            (
                state.epoch,
                state.indexed_epoch,
                match (&state.indexed_epoch, &state.dirty) {
                    (Some(_), Some(dirty)) => Some(dirty.clone()),
                    _ => None,
                },
            )
        };
        if indexed == Some(epoch) {
            return Ok(());
        }
        let frontier = ctx.indexer.meta.current_serial()?;
        match &scoped {
            Some(names) => self.apply_scoped(&ctx.indexer, names)?,
            None => self.write(&self.indexer.documents(&ctx.indexer)?)?,
        }
        self.retire_applied(scoped.as_ref(), epoch);
        ctx.indexer.meta.set_view_frontier(SEARCH_VIEW, frontier)?;
        Ok(())
    }

    fn apply_scoped(&self, ctx: &IndexerCtx<'_>, names: &BTreeMap<String, u64>) -> Result<(), SearchError> {
        let mut writer = self
            .index
            .writer_with_num_threads::<TantivyDocument>(1, WRITER_MEMORY_BYTES)?;
        for name in names.keys() {
            let update = self.indexer.resource_update(ctx, name)?;
            for key in &update.keys {
                writer.delete_term(Term::from_field_text(self.fields.key, key));
            }
            for resource in &update.documents {
                writer.add_document(self.document(resource))?;
            }
        }
        writer.commit()?;
        self.reader.reload()?;
        Ok(())
    }

    fn retire_applied(&self, applied: Option<&BTreeMap<String, u64>>, epoch: u64) {
        let mut state = self.state.lock().expect("search state lock");
        match applied {
            Some(generations) => {
                if let Some(dirty) = state.dirty.as_mut() {
                    dirty.retain(|name, generation| generations.get(name).is_none_or(|applied| *generation > *applied));
                }
            }
            None => {
                if state.epoch == epoch && state.dirty.is_none() {
                    state.dirty = Some(BTreeMap::new());
                }
            }
        }
        state.indexed_epoch = Some(epoch);
    }

    /// Callers advance the frontier only after every affected resource is current.
    ///
    /// # Errors
    /// Returns a search error if the writer cannot commit or the reader cannot reload.
    ///
    /// # Panics
    /// Panics if the rebuild lock was poisoned by a prior panic while rebuilding.
    pub fn update_resource(&self, docs: &[SearchDocument], key: &str) -> Result<(), SearchError> {
        let _guard = self.rebuild_lock.lock().expect("search rebuild lock");
        let mut writer = self
            .index
            .writer_with_num_threads::<TantivyDocument>(1, WRITER_MEMORY_BYTES)?;
        writer.delete_term(Term::from_field_text(self.fields.key, key));
        for resource in docs {
            writer.add_document(self.document(resource))?;
        }
        writer.commit()?;
        self.reader.reload()?;
        Ok(())
    }

    fn write(&self, documents: &[SearchDocument]) -> Result<(), SearchError> {
        let mut writer = self
            .index
            .writer_with_num_threads::<TantivyDocument>(1, WRITER_MEMORY_BYTES)?;
        writer.delete_all_documents()?;
        for resource in documents {
            writer.add_document(self.document(resource))?;
        }
        writer.commit()?;
        self.reader.reload()?;
        Ok(())
    }

    fn query(&self, params: &SearchParams, access: Option<&SearchAccess>) -> Result<Box<dyn Query>, SearchError> {
        let mut queries = vec![self.text_query(params.query.trim())?];
        if let Some(source) = params.source.content_source() {
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
        if params.availability.local_only() {
            queries.push(Box::new(TermQuery::new(
                Term::from_field_text(self.fields.available, AVAILABLE_LOCAL),
                IndexRecordOption::Basic,
            )));
        }
        if let Some(access) = access {
            queries.push(self.access_query(access)?);
        }
        Ok(if queries.len() == 1 {
            queries.pop().expect("query exists")
        } else {
            Box::new(BooleanQuery::intersection(queries))
        })
    }

    fn access_query(&self, access: &SearchAccess) -> Result<Box<dyn Query>, SearchError> {
        let mut queries = access
            .patterns
            .iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|SearchAccessPattern { route, glob }| {
                let route_query = Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.route, route),
                    IndexRecordOption::Basic,
                )) as Box<dyn Query>;
                RegexQuery::from_pattern(&glob_regex(glob), self.fields.normalized).map(|resource_query| {
                    Box::new(BooleanQuery::intersection(vec![route_query, Box::new(resource_query)])) as Box<dyn Query>
                })
            })
            .collect::<tantivy::Result<Vec<Box<dyn Query>>>>()?;
        Ok(match queries.len() {
            0 => Box::new(EmptyQuery),
            1 => queries.pop().expect("query exists"),
            _ => Box::new(BooleanQuery::union(queries)),
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
                &format!(".*{}.*", fold_lowercase(pattern)),
                self.fields.raw,
            )?));
        }
        let query = fold_lowercase(query);
        let terms = query_terms(&query);
        if terms.is_empty() {
            let pattern = format!(".*{}.*", escape_regex(&query));
            return Ok(Box::new(RegexQuery::from_pattern(&pattern, self.fields.raw)?));
        }
        let mut queries = terms
            .into_iter()
            .map(|term| {
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.search, &term),
                    IndexRecordOption::Basic,
                )) as Box<dyn Query>
            })
            .collect::<Vec<_>>();
        // N-grams do not preserve adjacency, so long queries require exact substring verification.
        if query.chars().count() > MAX_NGRAM {
            let pattern = format!(".*{}.*", escape_regex(&query));
            queries.push(Box::new(RegexQuery::from_pattern(&pattern, self.fields.raw)?));
        }
        Ok(Box::new(BooleanQuery::intersection(queries)))
    }

    fn result_from_doc(&self, doc: &TantivyDocument) -> SearchResult {
        SearchResult {
            display_label: stored_text(doc, self.fields.display),
            resource_key: stored_text(doc, self.fields.normalized),
            route: stored_text(doc, self.fields.route),
            index: stored_text(doc, self.fields.index),
            ecosystem: stored_text(doc, self.fields.ecosystem),
            type_label: String::new(),
            source_type: ContentSource::from_value(&stored_text(doc, self.fields.source))
                .expect("indexed source type is valid"),
            available_locally: stored_text(doc, self.fields.available) == AVAILABLE_LOCAL,
            summary: non_empty_string(stored_text(doc, self.fields.summary)),
        }
    }

    fn document(&self, resource: &SearchDocument) -> TantivyDocument {
        let sort = format!(
            "{}\u{0}{}\u{0}{}",
            resource.display_label.to_ascii_lowercase(),
            resource.route,
            resource.resource_key
        );
        let mut doc = TantivyDocument::new();
        doc.add_text(self.fields.key, document_key(&resource.route, &resource.resource_key));
        doc.add_text(self.fields.route, &resource.route);
        doc.add_text(self.fields.normalized, &resource.resource_key);
        doc.add_text(self.fields.display, &resource.display_label);
        doc.add_text(self.fields.source, resource.source.as_str());
        doc.add_text(
            self.fields.available,
            if resource.available_locally {
                AVAILABLE_LOCAL
            } else {
                AVAILABLE_REMOTE
            },
        );
        doc.add_text(self.fields.index, &resource.index);
        doc.add_text(self.fields.ecosystem, &resource.ecosystem);
        doc.add_text(self.fields.summary, resource.summary.as_deref().unwrap_or_default());
        doc.add_text(self.fields.sort, sort);
        doc.add_text(self.fields.search, &resource.text);
        doc.add_text(
            self.fields.raw,
            truncate_to_chars(&fold_lowercase(&resource.text), RAW_REGEX_BYTES),
        );
        doc
    }
}

struct RebuildSnapshot {
    epoch: u64,
    frontier: u64,
    documents: Vec<SearchDocument>,
}

#[derive(Default)]
struct IndexState {
    epoch: u64,
    indexed_epoch: Option<u64>,
    /// `None` distinguishes a full rebuild from an empty scoped update.
    dirty: Option<BTreeMap<String, u64>>,
}

#[derive(Clone, Copy)]
struct SearchFields {
    key: Field,
    route: Field,
    normalized: Field,
    display: Field,
    source: Field,
    available: Field,
    index: Field,
    ecosystem: Field,
    summary: Field,
    sort: Field,
    search: Field,
    raw: Field,
}

/// Indexing and scoped replacement must derive the same key.
#[must_use]
pub fn document_key(route: &str, normalized: &str) -> String {
    format!("{route}\u{0}{normalized}")
}

fn open_index(path: &Path, schema: &Schema) -> Result<TantivyIndex, SearchError> {
    Ok(TantivyIndex::builder()
        .schema(schema.clone())
        .tokenizers(tokenizers())
        .open_or_create(MmapDirectory::open(path)?)?)
}

fn reset_dir(path: &Path) -> std::io::Result<()> {
    std::fs::remove_dir_all(path)?;
    std::fs::create_dir_all(path)
}

fn rebuild_marker(path: &Path) -> PathBuf {
    path.with_extension("rebuilding")
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
        key: builder.add_text_field("key", exact.clone()),
        route: builder.add_text_field("route", exact.clone()),
        normalized: builder.add_text_field("normalized", exact.clone()),
        display: builder.add_text_field("display", stored.clone()),
        source: builder.add_text_field("source", exact.clone()),
        available: builder.add_text_field("available", exact),
        index: builder.add_text_field("index", stored.clone()),
        ecosystem: builder.add_text_field("ecosystem", stored.clone()),
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

fn fold_lowercase(value: &str) -> String {
    value.chars().flat_map(char::to_lowercase).collect()
}

fn query_terms(query: &str) -> Vec<String> {
    let chars: Vec<char> = query.chars().collect();
    match chars.len() {
        0 | 1 => Vec::new(),
        len if len <= MAX_NGRAM => vec![query.to_owned()],
        _ => chars
            .windows(MAX_NGRAM)
            .map(|term| term.iter().collect::<String>())
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
    push_escaped_regex(&mut escaped, value);
    escaped
}

fn glob_regex(value: &str) -> String {
    let mut pattern = String::with_capacity(value.len());
    let mut parts = value.split('*');
    push_escaped_regex(&mut pattern, parts.next().unwrap_or_default());
    for part in parts {
        pattern.push_str(".*");
        push_escaped_regex(&mut pattern, part);
    }
    pattern
}

fn push_escaped_regex(pattern: &mut String, value: &str) {
    for char in value.chars() {
        if REGEX_SPECIALS.contains(char) {
            pattern.push('\\');
        }
        pattern.push(char);
    }
}
