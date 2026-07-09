//! The ecosystem-indexer seam: turning stored records into searchable documents.

use std::sync::Arc;

use super::error::SearchError;
use super::params::PackageSource;
use crate::state::AppState;

pub const INDEXED_TEXT_BYTES: usize = 64 * 1024;

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

/// Runs several ecosystems' indexers and concatenates their documents, so a deployment serving more
/// than one ecosystem searches all of them without the neutral core knowing any ecosystem's vocabulary.
pub(super) struct CompositeIndexer(pub(super) Vec<Arc<dyn PackageIndexer>>);

impl PackageIndexer for CompositeIndexer {
    fn documents(&self, state: &AppState) -> Result<Vec<PackageDocument>, SearchError> {
        let mut documents = Vec::new();
        for indexer in &self.0 {
            documents.extend(indexer.documents(state)?);
        }
        Ok(documents)
    }
}

pub(super) fn default_indexer() -> Arc<dyn PackageIndexer> {
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
