//! What search reads from the running process, without reaching for the serving layer's state.

use peryx_core::{Ecosystem, Lexicon, LexiconRegistry};
use peryx_index::Index;
use peryx_storage::blob::BlobStore;
use peryx_storage::meta::MetaStore;

/// The stores and indexes an ecosystem's [`PackageIndexer`](crate::PackageIndexer) walks to derive
/// its documents.
///
/// An indexer needs the configured indexes and the two stores, and nothing else. Handing it exactly
/// that keeps it from reaching into the process's serving state, and keeps this crate below the HTTP
/// layer rather than inside it.
pub struct IndexerCtx<'a> {
    pub indexes: &'a [Index],
    pub meta: &'a MetaStore,
    pub blobs: &'a BlobStore,
}

impl IndexerCtx<'_> {
    /// The index at `position`, as a virtual index's `layers` name its members.
    #[must_use]
    pub fn index_at(&self, position: usize) -> &Index {
        &self.indexes[position]
    }
}

/// What one search request reads: the indexer's inputs, the mutation epoch that decides whether the
/// derived index is stale, and the vocabularies used to label each result.
pub struct SearchCtx<'a> {
    pub indexer: IndexerCtx<'a>,
    /// The process's mutation counter. The derived index rebuilds when it advances past the one the
    /// current documents were built from.
    pub epoch: u64,
    pub lexicons: &'a LexiconRegistry,
}

impl SearchCtx<'_> {
    /// The vocabulary for `ecosystem`, used to label a result in its own words.
    #[must_use]
    pub fn lexicon(&self, ecosystem: Ecosystem) -> &'static Lexicon {
        self.lexicons.get(ecosystem)
    }
}
