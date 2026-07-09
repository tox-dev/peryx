//! Package search over cached project metadata.

mod engine;
mod error;
mod indexer;
mod params;
mod response;

pub use engine::{PackageSearch, truncate_to_chars};
pub use error::SearchError;
pub use indexer::{EmptyIndexer, INDEXED_TEXT_BYTES, PackageDocument, PackageIndexer};
pub use params::{PackageSource, SearchParams, SourceFilter};
pub use response::{SearchResponse, SearchResult};
