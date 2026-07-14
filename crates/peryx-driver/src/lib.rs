//! The seam an ecosystem plugs into.
//!
//! An ecosystem crate implements one [`EcosystemDriver`](serving::EcosystemDriver) and nothing else;
//! where it mounts (per-index, or an absolute prefix like `OCI`'s `/v2/`) is data on the driver, not a
//! second trait. Everything a driver needs to do that lives here: the process
//! [`AppState`], the per-index route resolution it serves through, blob-download coordination, request
//! classification for rate limiting, and the discovery envelope.
//!
//! The router that dispatches to a driver sits *above* this crate, in `peryx-http`. An ecosystem
//! therefore never depends on the serving layer that hosts it, only on the seam it fills.

pub mod access;
pub mod body;
pub mod conditional;
pub mod discovery;
pub mod download;
mod driver_set;
pub mod openapi;
pub mod range;
pub mod rate_limit;
pub mod serving;
pub mod state;

pub use driver_set::DriverSet;
pub use state::{
    AppState, DEFAULT_HOT_CACHE_BYTES, DEFAULT_MAX_STALE_SECS, Index, IndexDescription, IndexKind, ServingState,
};

/// A `404 Not Found` with a plain body, the answer for a path no index or artifact owns.
#[must_use]
pub fn not_found() -> axum::response::Response {
    use axum::response::IntoResponse as _;
    (axum::http::StatusCode::NOT_FOUND, "not found").into_response()
}

#[cfg(test)]
mod tests;
