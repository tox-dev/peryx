//! The ecosystem-neutral tantivy index: schema, tokenizers, and query execution.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use tantivy::collector::{Count, TopDocs};
use tantivy::directory::MmapDirectory;
use tantivy::query::{AllQuery, BooleanQuery, Query, RegexQuery, TermQuery};
use tantivy::schema::document::{TantivyDocument, Value as _};
use tantivy::schema::{FAST, Field, IndexRecordOption, STORED, STRING, Schema, TextFieldIndexing, TextOptions};
use tantivy::tokenizer::{LowerCaser, NgramTokenizer, TextAnalyzer, TokenizerManager};
use tantivy::{Index as TantivyIndex, IndexReader, Order, Term};

use crate::context::SearchCtx;
use crate::error::SearchError;
use crate::indexer::{CompositeIndexer, PackageDocument, PackageIndexer, default_indexer};
use crate::params::{PackageSource, SearchParams};
use crate::response::{SearchResponse, SearchResult};

const SUBSTRING_TOKENIZER: &str = "peryx_substring";
const MIN_NGRAM: usize = 2;
const MAX_NGRAM: usize = 12;
const RAW_REGEX_BYTES: usize = 32 * 1024;
const WRITER_MEMORY_BYTES: usize = 64 * 1024 * 1024;
const REGEX_SPECIALS: &str = "\\.+*?()|[]{}^$";

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
    /// The index is a cache derived from the metadata store, so an index left by an earlier peryx
    /// whose schema no longer matches is discarded and rebuilt rather than failing startup. It
    /// repopulates as pages and tags are served.
    ///
    /// # Errors
    /// Returns an error if the directory cannot be created or read, or Tantivy cannot open the index
    /// for a reason other than a schema change.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SearchError> {
        let path = path.as_ref();
        std::fs::create_dir_all(path)?;
        let (schema, fields) = search_schema();
        let index = match open_index(path, &schema) {
            Err(SearchError::Tantivy(tantivy::TantivyError::SchemaError(_))) => {
                tracing::warn!(path = %path.display(), "search index schema changed; rebuilding it");
                reset_dir(path)?;
                open_index(path, &schema)?
            }
            result => result?,
        };
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

    /// Add another ecosystem's indexer, keeping any already installed. A second ecosystem composes its
    /// documents with the first rather than replacing them, so a mixed deployment searches every index.
    pub fn add_indexer(&mut self, indexer: Arc<dyn PackageIndexer>) {
        let current = std::mem::replace(&mut self.indexer, default_indexer());
        self.indexer = Arc::new(CompositeIndexer(vec![current, indexer]));
    }

    /// Search cached package documents.
    ///
    /// # Errors
    /// Returns an error if the derived index cannot refresh or the query is invalid.
    pub fn search(&self, ctx: &SearchCtx<'_>, params: SearchParams) -> Result<SearchResponse, SearchError> {
        self.ensure_current(ctx)?;
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
                searcher.doc::<TantivyDocument>(address).map(|doc| {
                    let mut result = self.result_from_doc(&doc);
                    let ecosystem = result.ecosystem.parse().unwrap_or_default();
                    ctx.lexicon(ecosystem).search_noun.clone_into(&mut result.type_label);
                    result
                })
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

    fn ensure_current(&self, ctx: &SearchCtx<'_>) -> Result<(), SearchError> {
        let epoch = ctx.epoch;
        let _guard = self.rebuild_lock.lock().expect("search rebuild lock");
        if self
            .indexed_epoch
            .lock()
            .expect("search epoch lock")
            .is_none_or(|indexed| indexed != epoch)
        {
            self.write(&self.indexer.documents(&ctx.indexer)?)?;
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
            ecosystem: stored_text(doc, self.fields.ecosystem),
            type_label: String::new(),
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
        doc.add_text(self.fields.ecosystem, &package.ecosystem);
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

#[derive(Clone, Copy)]
struct SearchFields {
    route: Field,
    normalized: Field,
    display: Field,
    source: Field,
    index: Field,
    ecosystem: Field,
    summary: Field,
    sort: Field,
    search: Field,
    raw: Field,
}

fn open_index(path: &Path, schema: &Schema) -> Result<TantivyIndex, SearchError> {
    Ok(TantivyIndex::builder()
        .schema(schema.clone())
        .tokenizers(tokenizers())
        .open_or_create(MmapDirectory::open(path)?)?)
}

/// Discard the on-disk index so a fresh one builds in its place. Drops the directory with whatever it
/// holds, then recreates it empty.
fn reset_dir(path: &Path) -> std::io::Result<()> {
    std::fs::remove_dir_all(path)?;
    std::fs::create_dir_all(path)
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
