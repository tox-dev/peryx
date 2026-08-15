//! HTTP byte-range handling follows [RFC 9110](https://www.rfc-editor.org/rfc/rfc9110) section 14.
//! Unsupported or malformed ranges produce a full response; unsatisfiable byte ranges produce `416`.

use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RangeRequest {
    /// `200` for an absent, unsupported, or malformed range.
    Whole,
    /// `206` with an end-exclusive byte range.
    Partial(Range<u64>),
    /// `416` for a valid range beyond the representation.
    Unsatisfiable,
}

/// Ignores unsupported, multipart, malformed, and reversed ranges as RFC 9110 requires. Clamps an
/// oversized end and treats a suffix longer than the representation as the whole representation.
#[must_use]
pub fn parse_range(header: Option<&str>, size: u64) -> RangeRequest {
    let Some(spec) = header.and_then(|value| value.strip_prefix("bytes=")) else {
        return RangeRequest::Whole;
    };
    let spec = spec.trim();
    // The response layer has no multipart representation.
    let Some((first, last)) = spec.split_once('-').filter(|_| !spec.contains(',')) else {
        return RangeRequest::Whole;
    };
    match (first.trim(), last.trim()) {
        ("", "") => RangeRequest::Whole,
        // RFC 9110 maps an oversized suffix to the whole representation; a zero suffix names no bytes.
        ("", suffix) => match suffix.parse::<u64>() {
            Ok(0) => RangeRequest::Unsatisfiable,
            Ok(_) if size == 0 => RangeRequest::Unsatisfiable,
            Ok(length) => RangeRequest::Partial(size.saturating_sub(length)..size),
            Err(_) => RangeRequest::Whole,
        },
        (first, "") => match first.parse::<u64>() {
            Ok(start) if start >= size => RangeRequest::Unsatisfiable,
            Ok(start) => RangeRequest::Partial(start..size),
            Err(_) => RangeRequest::Whole,
        },
        (first, last) => match (first.parse::<u64>(), last.parse::<u64>()) {
            // RFC 9110 treats a reversed interval as invalid syntax, not an unsatisfied range.
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
