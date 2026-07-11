//! Ecosystem-neutral package search over the derived index.
//!
//! The tantivy index, its schema, tokenizers and queries know nothing about any package format. Only
//! turning an index's stored records into searchable documents is ecosystem-specific, and that sits
//! behind the [`PackageIndexer`] seam, which each `peryx-ecosystem-*` crate implements.

mod context;
mod engine;
mod error;
mod indexer;
mod params;
mod response;

pub use context::{IndexerCtx, SearchCtx};
pub use engine::{PackageSearch, truncate_to_chars};
pub use error::SearchError;
pub use indexer::{EmptyIndexer, INDEXED_TEXT_BYTES, PackageDocument, PackageIndexer};
pub use params::{PackageSource, SearchParams, SourceFilter};
pub use response::{SearchResponse, SearchResult};

#[cfg(test)]
mod tests;
