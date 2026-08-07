//! The ecosystem-indexer seam: turning stored records into searchable documents.

use std::sync::Arc;

use crate::context::IndexerCtx;
use crate::engine::project_key;
use crate::error::SearchError;
use crate::params::PackageSource;

pub const INDEXED_TEXT_BYTES: usize = 64 * 1024;

/// One project's incremental search delta: the document keys to retire and the documents to add in
/// their place, so a mutation refreshes only the affected project rather than the whole index.
///
/// `keys` names every document the project could occupy across the configured indexes, so a project
/// that lost its last file retires its stale document even though it derives none; `documents` are the
/// freshly derived ones to add after the retirement. Delete-then-add over the same keys is idempotent,
/// so re-applying the same delta reaches the same index.
#[derive(Default)]
pub struct ProjectUpdate {
    pub keys: Vec<String>,
    pub documents: Vec<PackageDocument>,
}

/// Produces the search documents for one ecosystem's stored packages.
///
/// The tantivy index, schema, and querying are ecosystem-neutral; only turning an index's stored
/// records into searchable [`PackageDocument`]s is format-specific, so it sits behind this seam. Each
/// driver registers its own; a process with none wired in gets [`EmptyIndexer`].
pub trait PackageIndexer: Send + Sync {
    /// Every searchable document derivable from `ctx`, replacing the current index contents.
    ///
    /// # Errors
    /// Returns a search error when cached package records or blobs cannot be read.
    fn documents(&self, ctx: &IndexerCtx<'_>) -> Result<Vec<PackageDocument>, SearchError>;

    /// The incremental delta for the single project `name`: the keys to retire and the documents to add,
    /// across every index the project appears on.
    ///
    /// A mutation invalidates one project, so the lazy refresh derives only that project rather than the
    /// whole corpus. The default re-derives the full corpus and filters it, which is correct but no
    /// cheaper than a rebuild and cannot retire a project that lost its last document; an indexer that
    /// backs a mutation path overrides it to walk the one project and to name its keys even when it
    /// derives no document, so a deletion retires the stale one.
    ///
    /// # Errors
    /// Returns a search error when cached package records or blobs cannot be read.
    fn project_update(&self, ctx: &IndexerCtx<'_>, name: &str) -> Result<ProjectUpdate, SearchError> {
        let documents: Vec<PackageDocument> = self
            .documents(ctx)?
            .into_iter()
            .filter(|document| document.normalized_name == name)
            .collect();
        let keys = documents
            .iter()
            .map(|document| project_key(&document.route, &document.normalized_name))
            .collect();
        Ok(ProjectUpdate { keys, documents })
    }
}

/// The indexer installed until an ecosystem driver registers one: it yields no documents.
///
/// A process with no driver wired in searches an empty index, which keeps this crate free of any
/// format-specific indexing logic.
#[derive(Debug, Clone, Copy, Default)]
pub struct EmptyIndexer;

impl PackageIndexer for EmptyIndexer {
    fn documents(&self, _ctx: &IndexerCtx<'_>) -> Result<Vec<PackageDocument>, SearchError> {
        Ok(Vec::new())
    }
}

/// Runs several ecosystems' indexers and concatenates their documents, so a deployment serving more
/// than one ecosystem searches all of them without the neutral core knowing any ecosystem's vocabulary.
pub struct CompositeIndexer(pub(super) Vec<Arc<dyn PackageIndexer>>);

impl PackageIndexer for CompositeIndexer {
    fn documents(&self, ctx: &IndexerCtx<'_>) -> Result<Vec<PackageDocument>, SearchError> {
        let mut documents = Vec::new();
        for indexer in &self.0 {
            documents.extend(indexer.documents(ctx)?);
        }
        Ok(documents)
    }

    fn project_update(&self, ctx: &IndexerCtx<'_>, name: &str) -> Result<ProjectUpdate, SearchError> {
        let mut merged = ProjectUpdate::default();
        for indexer in &self.0 {
            let update = indexer.project_update(ctx, name)?;
            merged.keys.extend(update.keys);
            merged.documents.extend(update.documents);
        }
        Ok(merged)
    }
}

pub fn default_indexer() -> Arc<dyn PackageIndexer> {
    Arc::new(EmptyIndexer)
}

/// One searchable package, produced by a [`PackageIndexer`] and stored in the tantivy index. The
/// fields are ecosystem-neutral; the indexer decides how to fill them from its format's records.
pub struct PackageDocument {
    pub display_name: String,
    pub normalized_name: String,
    pub route: String,
    pub index: String,
    /// The lowercase ecosystem identifier of the owning index.
    pub ecosystem: String,
    pub source: PackageSource,
    /// Whether at least one of this package's artifacts can be served from local storage right now,
    /// computed by the indexer from stored placement metadata so a search filters on it without a
    /// per-result backend probe.
    pub available_locally: bool,
    pub summary: Option<String>,
    pub text: String,
}
