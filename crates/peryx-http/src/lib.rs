//! The HTTP layer: a read-through cache that serves the PEP 503/691 simple API and blob downloads,
//! fetching and caching from an upstream index on a miss.

pub mod body;
pub mod discovery;
pub mod download;
pub mod handlers;
pub mod path_safety;
pub mod rate_limit;
pub mod router;
pub mod serving;
pub mod state;

pub use router::router;
pub use state::{
    AppState, DEFAULT_HOT_CACHE_BYTES, DEFAULT_MAX_STALE_SECS, Index, IndexDescription, IndexKind, RuntimeOptions,
    describe_indexes,
};

#[cfg(test)]
mod tests;
