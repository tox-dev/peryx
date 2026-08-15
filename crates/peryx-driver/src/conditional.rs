//! HTTP validators for content-addressed artifacts.
//!
//! Digests provide strong validation. Dates support clients without entity tags. Validation runs
//! before opening a blob or starting an upstream fetch.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::http::{HeaderMap, header};

/// Applies the weak entity-tag comparison from RFC 9110 s13.1.2.
///
/// `*` matches an existing representation, and a list matches when any valid member matches. Invalid
/// members do not match.
#[must_use]
pub fn if_none_match(field: &str, etag: &str) -> bool {
    field
        .split(',')
        .map(str::trim)
        .any(|candidate| candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == etag)
}

/// Returns a range only when one `If-Range` value strongly matches `etag`.
///
/// Per RFC 9110 s13.1.5, stale, weak, date, or malformed validators suppress the range and produce a
/// full response rather than `416`. Multiple `Range` or `If-Range` fields are unsupported.
#[must_use]
pub fn applicable_range<'h>(headers: &'h HeaderMap, etag: &str) -> Option<&'h str> {
    let mut ranges = headers.get_all(header::RANGE).iter();
    let range = ranges.next()?.to_str().ok()?;
    if ranges.next().is_some() {
        return None;
    }
    let mut if_range = headers.get_all(header::IF_RANGE).iter();
    if_range.next().map_or(Some(range), |field| {
        (if_range.next().is_none() && field.to_str().is_ok_and(|field| field == etag)).then_some(range)
    })
}

/// Normalizes a last-modified time to whole seconds and clamps it to `now`.
///
/// Rounding up or returning a future time would prevent a matching `If-Modified-Since` from producing
/// `304 Not Modified`.
#[must_use]
pub fn last_modified(stored: SystemTime, now: SystemTime) -> SystemTime {
    let seconds = stored.min(now).duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    UNIX_EPOCH + Duration::from_secs(seconds)
}

#[must_use]
pub fn http_date(at: SystemTime) -> String {
    httpdate::fmt_http_date(at)
}

/// Applies the date comparison from RFC 9110 s13.1.3.
///
/// The condition holds when the representation is no newer than the supplied date. Invalid dates do
/// not match. The parser accepts all three HTTP date formats required by the RFC.
#[must_use]
pub fn if_modified_since(field: &str, modified: SystemTime) -> bool {
    httpdate::parse_http_date(field).is_ok_and(|since| modified <= since)
}
