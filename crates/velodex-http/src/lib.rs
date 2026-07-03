//! The HTTP layer: a read-through cache that serves the PEP 503/691 simple API and blob downloads,
//! fetching and caching from an upstream index on a miss.

pub mod api;
pub mod archive;
pub mod cache;
pub mod discovery;
pub mod handlers;
pub mod metrics;
pub mod path_safety;
pub mod rate_limit;
pub mod router;
pub mod search;
pub mod security;
pub mod state;
pub mod stream;
pub mod upload;
pub mod webhook;

pub use router::router;
pub use state::{AppState, Index, IndexDescription, IndexKind, RuntimeOptions, describe_indexes};

#[cfg(test)]
mod tests;
