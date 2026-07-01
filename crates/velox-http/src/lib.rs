//! The HTTP layer: a read-through cache that serves the PEP 503/691 simple API and blob downloads,
//! fetching and caching from an upstream index on a miss.

pub mod cache;
pub mod handlers;
pub mod router;
pub mod state;
pub mod upload;

pub use router::router;
pub use state::{AppState, StateConfig};

#[cfg(test)]
mod tests;
