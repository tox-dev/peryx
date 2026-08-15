//! Builds the response for an unsatisfiable blob range.
//!
//! [`peryx_storage::blob::parse_range`] owns parsing and returns
//! [`Unsatisfiable`](peryx_storage::blob::RangeRequest) for this response path.

use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::Response;

/// # Panics
/// Never in practice: the status and both header values are constructed here, not taken from a request.
#[must_use]
pub fn unsatisfiable_range(size: u64) -> Response {
    Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_RANGE, format!("bytes */{size}"))
        .body(Body::empty())
        .expect("range response builds from validated header parts")
}

#[cfg(test)]
#[path = "../tests/unit/range/tests.rs"]
mod tests;
