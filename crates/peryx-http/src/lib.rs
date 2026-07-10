//! The HTTP layer: the axum router, the neutral service endpoints, and the middleware that fronts
//! them.
//!
//! Requests resolve to a configured index and are handed to that index's ecosystem driver. The seam
//! the drivers implement, and the state they serve from, live in `peryx-driver` below this crate, so
//! no ecosystem depends on the router that dispatches to it.

pub mod handlers;
pub mod router;

pub use peryx_driver::state::{
    AppState, DEFAULT_HOT_CACHE_BYTES, DEFAULT_MAX_STALE_SECS, Index, IndexDescription, IndexKind, RuntimeOptions,
    describe_indexes,
};
pub use router::router;

#[cfg(test)]
mod tests;
