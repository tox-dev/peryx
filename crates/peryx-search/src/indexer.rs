use std::sync::Arc;

use crate::context::IndexerCtx;
use crate::engine::document_key;
use crate::error::SearchError;
use crate::params::ContentSource;

pub const INDEXED_TEXT_BYTES: usize = 65_536;

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
    pub text: String,
}
