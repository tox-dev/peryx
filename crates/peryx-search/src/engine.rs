//! The ecosystem-neutral tantivy index: schema, tokenizers, and query execution.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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
use crate::indexer::{CompositeIndexer, PackageDocument, PackageIndexer, default_indexer};
use crate::params::{PackageSource, SearchParams};
use crate::response::{SearchResponse, SearchResult};

const SUBSTRING_TOKENIZER: &str = "peryx_substring";
const MIN_NGRAM: usize = 2;
const MAX_NGRAM: usize = 12;
const RAW_REGEX_BYTES: usize = 32 * 1024;
const WRITER_MEMORY_BYTES: usize = 64 * 1024 * 1024;
const REGEX_SPECIALS: &str = "\\.+*?()|[]{}^$";
/// The term the `available` field carries for a locally available package; the availability filter
/// matches on it. Absence of this exact term is what excludes a remote-only or unavailable package.
const AVAILABLE_LOCAL: &str = "local";
const AVAILABLE_REMOTE: &str = "remote";

pub struct PackageSearch {
    index: TantivyIndex,
    reader: IndexReader,
    fields: SearchFields,
    indexer: Arc<dyn PackageIndexer>,
    epoch: AtomicU64,
    indexed_epoch: Mutex<Option<u64>>,
    /// The projects a lazy refresh must re-derive, or `None` when the whole index must rebuild. A scoped
    /// mutation records only its project; a blanket invalidation ([`bump_epoch`](PackageSearch::bump_epoch))
    /// or a not-yet-populated index clears it to `None`, so the next refresh re-derives everything.
    dirty: Mutex<Option<BTreeSet<String>>>,
    rebuild_lock: Mutex<()>,
    /// The on-disk index directory, or `None` for an in-memory index. An eager rebuild uses it to
    /// mark an in-flight rebuild so a restart that interrupts one discards the partial index.
    home: Option<PathBuf>,
}

/// How far an eager [`rebuild`](PackageSearch::rebuild) has progressed, reported once per staged chunk
/// so a caller can surface operator progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RebuildProgress {
    /// Documents staged into the candidate generation so far.
    pub indexed: u64,
    /// Documents the rebuild will stage in total.
    pub total: u64,
}

/// How an eager [`rebuild`](PackageSearch::rebuild) ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebuildOutcome {
    /// The rebuilt index replaced the served one; `documents` were published by the single closing commit.
    Published { documents: u64 },
    /// The caller cancelled before publication; the served index kept its prior contents. `documents`
    /// counts the chunks staged before the abort, all discarded with the rolled-back candidate generation.
    Aborted { documents: u64 },
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
            None,
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
            epoch: AtomicU64::new(0),
            indexed_epoch: Mutex::new(None),
            dirty: Mutex::new(None),
            rebuild_lock: Mutex::new(()),
            home,
        })
    }

    /// Add another ecosystem's indexer, keeping any already installed. A second ecosystem composes its
    /// documents with the first rather than replacing them, so a mixed deployment searches every index.
    pub fn add_indexer(&mut self, indexer: Arc<dyn PackageIndexer>) {
        let current = std::mem::replace(&mut self.indexer, default_indexer());
        self.indexer = Arc::new(CompositeIndexer(vec![current, indexer]));
    }

    /// Invalidate the whole derived index after a mutation whose affected projects are not known, so the
    /// next search re-derives every document. Reserve it for that case; a mutation that names its project
    /// should call [`invalidate_project`](PackageSearch::invalidate_project) to refresh only that one.
    ///
    /// # Panics
    /// Panics if the dirty-set lock was poisoned by a prior panic while holding it.
    pub fn bump_epoch(&self) {
        *self.dirty.lock().expect("search dirty lock") = None;
        self.epoch.fetch_add(1, Ordering::Release);
    }

    /// Invalidate one project after a mutation that changed its searchable documents, so the next search
    /// re-derives and rewrites only that project instead of the whole corpus.
    ///
    /// While a blanket rebuild is already pending the project folds into it rather than being tracked
    /// separately, since the rebuild re-derives everything regardless.
    ///
    /// # Panics
    /// Panics if the dirty-set lock was poisoned by a prior panic while holding it.
    pub fn invalidate_project(&self, name: &str) {
        if let Some(dirty) = self.dirty.lock().expect("search dirty lock").as_mut() {
            dirty.insert(name.to_owned());
        }
        self.epoch.fetch_add(1, Ordering::Release);
    }

    /// Rebuild the whole index from authoritative metadata, committing in chunks and publishing the
    /// result atomically.
    ///
    /// Unlike the lazy refresh a search triggers, this is an eager operator recovery path for when the
    /// derived index falls behind: it re-derives every document and stages them into the writer in
    /// `chunk` batches, whose flushing bounds peak writer memory, then commits once at the end and
    /// reloads the reader. Staging never commits, so the candidate generation stays out of the live
    /// Tantivy index until that single closing commit publishes it atomically. Concurrent searches keep
    /// serving the prior complete index until then, and a cancellation rolls the candidate generation
    /// back so no scoped [`update_project`](PackageSearch::update_project) can later reload its writes.
    /// On disk, a marker records the in-flight rebuild, so a restart that interrupts one discards any
    /// staged state and starts over rather than serving it.
    ///
    /// The walk over authoritative metadata that derives every document runs off the writer lock: it
    /// reads the metadata store, not the index, so a scoped [`update_project`](PackageSearch::update_project)
    /// or a lazy refresh no longer waits behind the whole eager rebuild. The lock is taken only for the
    /// writer work that publishes the result. A mutation that advanced the store serial while the walk
    /// ran could have written a scoped update the off-lock snapshot missed; re-deriving once under the
    /// lock, where no scoped writer can interleave, keeps a stale full write from clobbering it.
    ///
    /// `observe` is called before each chunk with the running progress; returning
    /// [`ControlFlow::Break`] cancels the rebuild, leaving the served index untouched.
    ///
    /// # Errors
    /// Returns a search error if the documents cannot be derived, the writer cannot commit, or the
    /// in-flight marker cannot be written.
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
                return Ok(RebuildOutcome::Aborted { documents: indexed });
            }
            for package in slice {
                writer.add_document(self.document(package))?;
            }
            indexed += slice.len() as u64;
        }
        let _ = observe(RebuildProgress { indexed, total });
        writer.commit()?;
        self.reader.reload()?;
        self.clear_rebuilding();
        // The rebuild re-derived every project as of `snapshot.epoch`, so unless a mutation has advanced
        // the epoch since, the scoped tracker starts empty and later mutations refresh only their project.
        if self.epoch.load(Ordering::Acquire) == snapshot.epoch {
            *self.dirty.lock().expect("search dirty lock") = Some(BTreeSet::new());
        }
        *self.indexed_epoch.lock().expect("search epoch lock") = Some(snapshot.epoch);
        ctx.meta.set_view_frontier(SEARCH_VIEW, snapshot.frontier)?;
        Ok(RebuildOutcome::Published { documents: indexed })
    }

    /// Derive the whole document set with the mutation epoch and store serial it reflects, off the
    /// writer lock. The serial is read before the walk so it never names metadata the walk missed, which
    /// lets [`rebuild`](PackageSearch::rebuild) detect a snapshot a concurrent mutation raced.
    fn snapshot(&self, ctx: &IndexerCtx<'_>) -> Result<RebuildSnapshot, SearchError> {
        let epoch = self.epoch.load(Ordering::Relaxed);
        let frontier = ctx.meta.current_serial()?;
        let documents = self.indexer.documents(ctx)?;
        Ok(RebuildSnapshot {
            epoch,
            frontier,
            documents,
        })
    }

    /// Record that an on-disk rebuild is in flight, so an interrupted rebuild is discarded on restart.
    fn mark_rebuilding(&self) -> Result<(), SearchError> {
        if let Some(home) = &self.home {
            std::fs::write(rebuild_marker(home), [])?;
        }
        Ok(())
    }

    /// Clear the in-flight marker after a rebuild publishes. Removal is best-effort: a marker left
    /// behind only makes the next restart rebuild an already-complete index, never serve a partial one.
    fn clear_rebuilding(&self) {
        if let Some(home) = &self.home {
            let _ = std::fs::remove_file(rebuild_marker(home));
        }
    }

    /// Search cached package documents.
    ///
    /// # Errors
    /// Returns an error if the derived index cannot refresh or the query is invalid.
    pub fn search(&self, ctx: &SearchCtx<'_>, params: SearchParams) -> Result<SearchResponse, SearchError> {
        self.search_with_access(ctx, params, None)
    }

    /// Apply access inside the query so totals and pages contain only readable resources.
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
                searcher.doc::<TantivyDocument>(address).map(|doc| {
                    let mut result = self.result_from_doc(&doc);
                    let ecosystem = result.ecosystem.parse().expect("indexed ecosystem identity is valid");
                    ctx.lexicon(ecosystem).search_noun.clone_into(&mut result.type_label);
                    result
                })
            })
            .collect::<tantivy::Result<Vec<_>>>()?;
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
        // A held lock means an eager rebuild or another refresh is running; serve the current reader
        // rather than block or race a second writer. An eager rebuild leaves the reader on the prior
        // complete index until it publishes, so this serves complete results, never partial ones.
        let Ok(_guard) = self.rebuild_lock.try_lock() else {
            return Ok(());
        };
        let epoch = self.epoch.load(Ordering::Acquire);
        let indexed = *self.indexed_epoch.lock().expect("search epoch lock");
        if indexed == Some(epoch) {
            return Ok(());
        }
        // A not-yet-populated index or a blanket invalidation (`dirty` cleared to `None`) rebuilds every
        // document; otherwise the snapshot of dirty projects re-derives only those. The snapshot is read
        // before the write and the applied names are retired after it, so a mutation that lands mid-write
        // survives: it re-bumps the epoch, leaving `indexed` behind so the next search re-derives it.
        let scoped = match (indexed, self.dirty.lock().expect("search dirty lock").as_ref()) {
            (Some(_), Some(dirty)) => Some(dirty.clone()),
            _ => None,
        };
        let frontier = ctx.indexer.meta.current_serial()?;
        match &scoped {
            Some(names) => self.apply_scoped(&ctx.indexer, names)?,
            None => self.write(&self.indexer.documents(&ctx.indexer)?)?,
        }
        self.retire_applied(scoped.as_ref(), epoch);
        *self.indexed_epoch.lock().expect("search epoch lock") = Some(epoch);
        ctx.indexer.meta.set_view_frontier(SEARCH_VIEW, frontier)?;
        Ok(())
    }

    /// Re-derive and rewrite only `names`, retiring each project's prior documents and adding its fresh
    /// ones in a single writer commit, then reloading the reader once.
    fn apply_scoped(&self, ctx: &IndexerCtx<'_>, names: &BTreeSet<String>) -> Result<(), SearchError> {
        let mut writer = self
            .index
            .writer_with_num_threads::<TantivyDocument>(1, WRITER_MEMORY_BYTES)?;
        for name in names {
            let update = self.indexer.project_update(ctx, name)?;
            for key in &update.keys {
                writer.delete_term(Term::from_field_text(self.fields.key, key));
            }
            for package in &update.documents {
                writer.add_document(self.document(package))?;
            }
        }
        writer.commit()?;
        self.reader.reload()?;
        Ok(())
    }

    /// Clear the just-applied invalidation once its write succeeded. A scoped write drops only the names
    /// it wrote, leaving any that arrived mid-write; a full write, when no mutation has advanced the epoch
    /// since it started, resets the tracker to empty so later mutations refresh only their own project.
    fn retire_applied(&self, applied: Option<&BTreeSet<String>>, epoch: u64) {
        let mut dirty = self.dirty.lock().expect("search dirty lock");
        match applied {
            Some(names) => {
                if let Some(tracked) = dirty.as_mut() {
                    for name in names {
                        tracked.remove(name);
                    }
                }
            }
            None => {
                if self.epoch.load(Ordering::Acquire) == epoch && dirty.is_none() {
                    *dirty = Some(BTreeSet::new());
                }
            }
        }
    }

    /// Replace one project's document on one index — the record keyed by [`project_key`] — with `docs`,
    /// leaving every other project untouched.
    ///
    /// A replica calls this as it applies a metadata page: it retires the project's stale document by
    /// its exact key term and re-adds the freshly derived one (or none, when the project no longer has
    /// files), then reloads the reader. Unlike [`write`](PackageSearch::write) and
    /// [`rebuild`](PackageSearch::rebuild) it touches neither the mutation epoch nor the view frontier,
    /// so the caller sequences the frontier advance after every affected project is current. Delete then
    /// add is idempotent, so re-running the same update after a crash reaches the same index.
    ///
    /// It shares the writer lock with an eager rebuild so the two never open a second writer over the
    /// same directory; a search that finds the lock held serves the current reader rather than blocking.
    ///
    /// # Errors
    /// Returns a search error if the writer cannot commit or the reader cannot reload.
    ///
    /// # Panics
    /// Panics if the rebuild lock was poisoned by a prior panic while rebuilding.
    pub fn update_project(&self, docs: &[PackageDocument], key: &str) -> Result<(), SearchError> {
        let _guard = self.rebuild_lock.lock().expect("search rebuild lock");
        let mut writer = self
            .index
            .writer_with_num_threads::<TantivyDocument>(1, WRITER_MEMORY_BYTES)?;
        writer.delete_term(Term::from_field_text(self.fields.key, key));
        for package in docs {
            writer.add_document(self.document(package))?;
        }
        writer.commit()?;
        self.reader.reload()?;
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

    fn query(&self, params: &SearchParams, access: Option<&SearchAccess>) -> Result<Box<dyn Query>, SearchError> {
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
                RegexQuery::from_pattern(&glob_regex(glob), self.fields.normalized).map(|project_query| {
                    Box::new(BooleanQuery::intersection(vec![route_query, Box::new(project_query)])) as Box<dyn Query>
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
        // A query over MAX_NGRAM characters is split into overlapping grams AND-combined here, but the
        // n-gram index enforces neither adjacency nor order, so a document can satisfy every gram in
        // separate spans without containing the query. The grams stay a prefilter; a regex over the raw
        // field verifies the complete substring, so totals, pages, and ordering count only true matches.
        if query.chars().count() > MAX_NGRAM {
            let pattern = format!(".*{}.*", escape_regex(&query));
            queries.push(Box::new(RegexQuery::from_pattern(&pattern, self.fields.raw)?));
        }
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
            available_locally: stored_text(doc, self.fields.available) == AVAILABLE_LOCAL,
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
        doc.add_text(self.fields.key, project_key(&package.route, &package.normalized_name));
        doc.add_text(self.fields.route, &package.route);
        doc.add_text(self.fields.normalized, &package.normalized_name);
        doc.add_text(self.fields.display, &package.display_name);
        doc.add_text(self.fields.source, package.source.as_str());
        doc.add_text(
            self.fields.available,
            if package.available_locally {
                AVAILABLE_LOCAL
            } else {
                AVAILABLE_REMOTE
            },
        );
        doc.add_text(self.fields.index, &package.index);
        doc.add_text(self.fields.ecosystem, &package.ecosystem);
        doc.add_text(self.fields.summary, package.summary.as_deref().unwrap_or_default());
        doc.add_text(self.fields.sort, sort);
        doc.add_text(self.fields.search, &package.text);
        doc.add_text(
            self.fields.raw,
            truncate_to_chars(&fold_lowercase(&package.text), RAW_REGEX_BYTES),
        );
        doc
    }
}

/// A document set derived off the writer lock, tagged with the mutation epoch and store serial it
/// reflects so [`rebuild`](PackageSearch::rebuild) can tell whether a concurrent mutation raced it.
struct RebuildSnapshot {
    epoch: u64,
    frontier: u64,
    documents: Vec<PackageDocument>,
}

#[derive(Clone, Copy)]
struct SearchFields {
    /// The scoped-update handle: `{route}\0{normalized}`, one exact term per document so a per-project
    /// rebuild deletes exactly that project's document on one index and re-adds the fresh one, leaving
    /// every other project untouched.
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

/// The scoped-update key for `normalized` as served on `route`.
///
/// It is the exact term [`update_project`](PackageSearch::update_project) deletes and the document
/// carries, so a caller rebuilding one project on one index names it through this and the two never drift.
#[must_use]
pub fn project_key(route: &str, normalized: &str) -> String {
    format!("{route}\u{0}{normalized}")
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

/// The sibling file that marks an in-flight rebuild of the index at `path`. It sits beside the index
/// directory rather than inside it so tantivy's own file management never touches it.
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

/// Fold to lowercase the way the substring index does, so an accented or non-Latin query matches the
/// text it indexed. Tantivy's `LowerCaser` maps each character through `char::to_lowercase`; matching
/// it needs the same per-character fold, not `to_ascii_lowercase` (which leaves non-ASCII letters
/// uppercase) nor `str::to_lowercase` (which special-cases final sigma).
fn fold_lowercase(value: &str) -> String {
    value.chars().flat_map(char::to_lowercase).collect()
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
