//! Resolve an HTTP `Range` header against a stored blob of a known size.
//!
//! Every blob server leans on the same grammar: pulling one layer of a large image is a range request,
//! and clients use one to resume interrupted artifact downloads. [`parse_range`] maps a request's `Range`
//! value and the blob's total size to one of three answers a handler acts on: serve the whole blob,
//! serve one end-exclusive byte range, or reject as unsatisfiable.
//!
//! It follows [RFC 9110](https://www.rfc-editor.org/rfc/rfc9110) section 14. An absent header, an
//! unknown range unit, a multi-range set, or a malformed spec is ignored and the whole blob is served,
//! because a server that cannot apply a `Range` answers `200` with the full representation rather than
//! failing. A reversed range (`last` before `first`) is invalid syntax, so it too is ignored. Only a
//! well-formed single `bytes=` range whose first byte sits at or past the end is unsatisfiable, the
//! `416` signal; the end is clamped to the blob when it overshoots.

use std::ops::Range;

/// What a handler should serve for a request's `Range` header against a blob of a known size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RangeRequest {
    /// Serve the whole blob (`200`): no `Range`, an unrecognized one, or one to ignore per RFC 9110.
    Whole,
    /// Serve this end-exclusive byte range (`206`).
    Partial(Range<u64>),
    /// The `Range` is well formed but names bytes the blob does not have (`416`).
    Unsatisfiable,
}

/// Resolve a request's `Range` header against a blob of `size` bytes.
///
/// An absent header, a non-`bytes=` unit, a comma-joined multi-range, a malformed spec, or a reversed
/// range all resolve to [`RangeRequest::Whole`]: RFC 9110 has a server ignore a `Range` it cannot apply
/// and serve the full representation. A single `bytes=` range resolves to [`RangeRequest::Partial`] with
/// its end clamped to the blob, or to [`RangeRequest::Unsatisfiable`] when the first byte sits at or past
/// the end. A suffix longer than the blob names the whole blob, since RFC 9110 uses the entire
/// representation when it is shorter than the requested suffix.
#[must_use]
pub fn parse_range(header: Option<&str>, size: u64) -> RangeRequest {
    let Some(spec) = header.and_then(|value| value.strip_prefix("bytes=")) else {
        return RangeRequest::Whole;
    };
    let spec = spec.trim();
    // No caller answers multipart, so a multi-range request is one this server does not speak.
    let Some((first, last)) = spec.split_once('-').filter(|_| !spec.contains(',')) else {
        return RangeRequest::Whole;
    };
    match (first.trim(), last.trim()) {
        ("", "") => RangeRequest::Whole,
        // `bytes=-N`: the last N bytes. A suffix past the size names the whole blob; a zero-length
        // suffix names nothing a well-formed range can hold.
        ("", suffix) => match suffix.parse::<u64>() {
            Ok(0) => RangeRequest::Unsatisfiable,
            // An empty representation has no bytes for a suffix to name.
            Ok(_) if size == 0 => RangeRequest::Unsatisfiable,
            Ok(length) => RangeRequest::Partial(size.saturating_sub(length)..size),
            Err(_) => RangeRequest::Whole,
        },
        // `bytes=N-`: from N to the end.
        (first, "") => match first.parse::<u64>() {
            Ok(start) if start >= size => RangeRequest::Unsatisfiable,
            Ok(start) => RangeRequest::Partial(start..size),
            Err(_) => RangeRequest::Whole,
        },
        (first, last) => match (first.parse::<u64>(), last.parse::<u64>()) {
            // `last < first` reads backwards, so it is not a range at all.
            (Ok(first), Ok(last)) if first > last => RangeRequest::Whole,
            (Ok(first), Ok(_)) if first >= size => RangeRequest::Unsatisfiable,
            (Ok(first), Ok(last)) => RangeRequest::Partial(first..last.min(size - 1) + 1),
            _ => RangeRequest::Whole,
        },
    }
}

#[cfg(test)]
#[path = "../../tests/unit/blob/range/tests.rs"]
mod tests;
