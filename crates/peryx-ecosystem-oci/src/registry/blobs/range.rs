//! HTTP single-range requests over a blob of known size.
//!
//! Pulling one layer of a large image is often a range request, so this is the grammar the blob
//! server leans on.

use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::Response;

/// The `416` a well-formed but unmeetable range earns, naming the size the client should retry against.
pub(super) fn unsatisfiable_range(size: u64) -> Response {
    Response::builder()
        .status(StatusCode::RANGE_NOT_SATISFIABLE)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_RANGE, format!("bytes */{size}"))
        .body(Body::empty())
        .expect("range response builds from validated header parts")
}
/// What a `Range` header asks of a representation of a known size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RangeSpec {
    /// Not a range this server understands. RFC 9110 s14.2: an unparseable `Range` is ignored and
    /// the whole representation served, never refused.
    Ignore,
    /// A well-formed range that no part of the representation can satisfy: a `416`.
    Unsatisfiable,
    /// An inclusive byte range within the representation.
    Satisfiable(u64, u64),
}

/// Parse a single-range `Range: bytes=...` header against a known size, inclusive per HTTP semantics.
///
/// The distinction that matters is between a range that is *wrong* and one that is *unmeetable*.
/// `bytes=abc-` is not a range at all, so it is ignored; `bytes=9999-` against a 10-byte blob is a
/// range this blob cannot satisfy, so it earns a `416`. Answering `416` to the former tells a client
/// its perfectly good request was out of bounds.
pub(super) fn parse_range(header: &str, size: u64) -> RangeSpec {
    let Some(spec) = header.strip_prefix("bytes=") else {
        return RangeSpec::Ignore;
    };
    let Some((first, last)) = spec.split_once('-') else {
        return RangeSpec::Ignore;
    };
    let Some(end_of_blob) = size.checked_sub(1) else {
        // An empty representation satisfies no range, but a well-formed one still earns a `416`.
        return RangeSpec::Unsatisfiable;
    };
    match (first.is_empty(), last.is_empty()) {
        // `bytes=-N`: the last N bytes. RFC 9110 s14.1.2: when N exceeds the size, the whole
        // representation is used rather than the range being refused.
        (true, false) => match last.parse::<u64>() {
            Ok(0) => RangeSpec::Unsatisfiable,
            Ok(suffix) => RangeSpec::Satisfiable(size.saturating_sub(suffix), end_of_blob),
            Err(_) => RangeSpec::Ignore,
        },
        // `bytes=N-`: from N to the end.
        (false, true) => match first.parse::<u64>() {
            Ok(start) if start > end_of_blob => RangeSpec::Unsatisfiable,
            Ok(start) => RangeSpec::Satisfiable(start, end_of_blob),
            Err(_) => RangeSpec::Ignore,
        },
        (false, false) => match (first.parse::<u64>(), last.parse::<u64>()) {
            // `last < first` is not a range; nothing can be read backwards.
            (Ok(start), Ok(end)) if start > end => RangeSpec::Ignore,
            (Ok(start), Ok(_)) if start > end_of_blob => RangeSpec::Unsatisfiable,
            (Ok(start), Ok(end)) => RangeSpec::Satisfiable(start, end.min(end_of_blob)),
            _ => RangeSpec::Ignore,
        },
        (true, true) => RangeSpec::Ignore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_range_reads_a_satisfiable_span() {
        assert_eq!(parse_range("bytes=0-3", 10), RangeSpec::Satisfiable(0, 3));
        assert_eq!(parse_range("bytes=5-", 10), RangeSpec::Satisfiable(5, 9));
        assert_eq!(parse_range("bytes=-3", 10), RangeSpec::Satisfiable(7, 9));
        // An end past the last byte is clamped, not refused.
        assert_eq!(parse_range("bytes=8-99", 10), RangeSpec::Satisfiable(8, 9));
    }

    #[test]
    fn test_parse_range_ignores_what_is_not_a_range() {
        // RFC 9110 s14.2: an unparseable `Range` is ignored, and the whole representation served.
        for header in [
            "bytes=",
            "bytes=-",
            "bytes=abc-",
            "bytes=-xyz",
            "bytes=5-2",
            "items=0-1",
            // One half of the pair reads and the other does not; still not a range.
            "bytes=1-abc",
            "bytes=abc-9",
        ] {
            assert_eq!(parse_range(header, 10), RangeSpec::Ignore, "{header}");
        }
    }

    #[test]
    fn test_parse_range_refuses_only_what_the_blob_cannot_meet() {
        assert_eq!(parse_range("bytes=10-", 10), RangeSpec::Unsatisfiable);
        assert_eq!(parse_range("bytes=99-100", 10), RangeSpec::Unsatisfiable);
        assert_eq!(parse_range("bytes=-0", 10), RangeSpec::Unsatisfiable);
        assert_eq!(parse_range("bytes=0-0", 0), RangeSpec::Unsatisfiable);
    }

    #[test]
    fn test_parse_range_serves_the_whole_blob_for_an_oversized_suffix() {
        // RFC 9110 s14.1.2: a suffix longer than the representation uses the entire representation.
        assert_eq!(parse_range("bytes=-99", 10), RangeSpec::Satisfiable(0, 9));
    }
}
