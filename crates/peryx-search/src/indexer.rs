use std::sync::Arc;

use tantivy::tokenizer::MAX_TOKEN_LEN;

use crate::context::IndexerCtx;
use crate::engine::document_key;
use crate::error::SearchError;
use crate::params::ContentSource;

/// The window every document is folded and truncated to.
///
/// Both matching paths read exactly it: the n-gram prefilter over the window, exact verification
/// over its columnar copy. Tantivy's token ceiling sets the width, since the prefilter tokenizes it.
pub const INDEXED_TEXT_BYTES: usize = MAX_TOKEN_LEN;

#[derive(Default)]
pub struct ResourceUpdate {
    pub keys: Vec<String>,
    pub documents: Vec<SearchDocument>,
}

pub trait SearchDocumentProvider: Send + Sync {
    /// Must return the complete replacement set.
    ///
    /// # Errors
    /// Returns a search error when cached resource records or blobs cannot be read.
    fn documents(&self, ctx: &IndexerCtx<'_>) -> Result<Vec<SearchDocument>, SearchError>;

    /// Providers should override this when they can identify stale keys after all artifacts disappear.
    ///
    /// # Errors
    /// Returns a search error when cached resource records or blobs cannot be read.
    fn resource_update(&self, ctx: &IndexerCtx<'_>, name: &str) -> Result<ResourceUpdate, SearchError> {
        let documents: Vec<SearchDocument> = self
            .documents(ctx)?
            .into_iter()
            .filter(|document| document.resource_key == name)
            .collect();
        let keys = documents
            .iter()
            .map(|document| document_key(&document.route, &document.resource_key))
            .collect();
        Ok(ResourceUpdate { keys, documents })
    }
}

pub struct CompositeIndexer(pub(super) Vec<Arc<dyn SearchDocumentProvider>>);

impl SearchDocumentProvider for CompositeIndexer {
    fn documents(&self, ctx: &IndexerCtx<'_>) -> Result<Vec<SearchDocument>, SearchError> {
        let mut documents = Vec::new();
        for indexer in &self.0 {
            documents.extend(indexer.documents(ctx)?);
        }
        Ok(documents)
    }

    fn resource_update(&self, ctx: &IndexerCtx<'_>, name: &str) -> Result<ResourceUpdate, SearchError> {
        let mut merged = ResourceUpdate::default();
        for indexer in &self.0 {
            let update = indexer.resource_update(ctx, name)?;
            merged.keys.extend(update.keys);
            merged.documents.extend(update.documents);
        }
        Ok(merged)
    }
}

#[must_use]
pub fn default_indexer() -> Arc<dyn SearchDocumentProvider> {
    Arc::new(CompositeIndexer(Vec::new()))
}

pub struct SearchDocument {
    pub display_label: String,
    pub resource_key: String,
    pub route: String,
    pub index: String,
    pub ecosystem: String,
    pub source: ContentSource,
    /// Precomputed to avoid storage probes while querying.
    pub available_locally: bool,
    pub summary: Option<String>,
    /// Indexing folds case and truncates this at [`INDEXED_TEXT_BYTES`]; a provider that budgets sections
    /// chooses what reaches that window rather than widening it.
    pub text: String,
}
