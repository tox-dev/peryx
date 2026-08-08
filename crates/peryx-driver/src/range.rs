//! The HTTP `416` response for a range a stored file cannot satisfy.
//!
//! Parsing a `Range` header against a blob's size lives in
//! [`peryx_storage::blob::parse_range`](peryx_storage::blob::parse_range); this is the response side a
//! handler reaches for when that parse comes back [`Unsatisfiable`](peryx_storage::blob::RangeRequest).

use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::Response;

/// The `416` a well-formed but unmeetable range earns, naming the size the client should retry against.
///
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
