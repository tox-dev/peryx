//! Ecosystem-neutral package search over the derived index.
//!
//! The tantivy index, its schema, tokenizers and queries know nothing about any package format. Only
//! turning an index's stored records into searchable documents is ecosystem-specific, and that sits
//! behind the [`PackageIndexer`] seam, which each `peryx-ecosystem-*` crate implements.

mod access;
mod context;
mod engine;
mod error;
mod indexer;
mod params;
mod response;

pub use access::{SearchAccess, SearchAccessPattern};

/// The derived-view name the search index records its applied metadata frontier under, so a replica's
/// readable-frontier calculation waits on the index before it exposes newer metadata.
pub const SEARCH_VIEW: &str = "search";
pub use context::{IndexerCtx, SearchCtx};
pub use engine::{PackageSearch, RebuildOutcome, RebuildProgress, project_key, truncate_to_chars};
pub use error::SearchError;
pub use indexer::{EmptyIndexer, INDEXED_TEXT_BYTES, PackageDocument, PackageIndexer, ProjectUpdate};
pub use params::{AvailabilityFilter, PackageSource, SearchParams, SourceFilter};
pub use response::{SearchResponse, SearchResult};

#[cfg(test)]
#[path = "../tests/unit/tests/mod.rs"]
mod tests;
