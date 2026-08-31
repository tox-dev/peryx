mod access;
mod context;
mod engine;
mod error;
mod indexer;
mod params;
mod response;
mod verify;

pub use access::{SearchAccess, SearchAccessPattern};

/// Replica reads use this frontier to stay behind indexed metadata.
pub const SEARCH_VIEW: &str = "search";
pub use context::{IndexerCtx, SearchCtx};
pub use engine::{RebuildOutcome, RebuildProgress, SearchIndex, document_key, truncate_to_chars};
pub use error::SearchError;
pub use indexer::{INDEXED_TEXT_BYTES, ResourceUpdate, SearchDocument, SearchDocumentProvider, default_indexer};
pub use params::{AvailabilityFilter, ContentSource, SearchParams, SourceFilter};
pub use response::{SearchResponse, SearchResult};

#[cfg(test)]
#[path = "../tests/unit/tests/mod.rs"]
mod tests;
