//! The ecosystem-indexer seam: turning stored records into searchable documents.

use std::sync::Arc;

use crate::context::IndexerCtx;
use crate::error::SearchError;
use crate::params::PackageSource;

pub const INDEXED_TEXT_BYTES: usize = 64 * 1024;

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
    /// The lowercase ecosystem identifier of the owning index (`pypi`, `oci`).
    pub ecosystem: String,
    pub source: PackageSource,
    pub summary: Option<String>,
    pub text: String,
}
