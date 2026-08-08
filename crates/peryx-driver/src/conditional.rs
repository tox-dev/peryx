//! The validators of a content-addressed representation: its entity tag and its modification date.
//!
//! The verified digest names an artifact's bytes, so it is the strong validator for them. A client
//! holding those bytes gets its answer from the request line, with no blob opened and no upstream
//! fetch started, which is why this sits next to the range grammar both blob servers lean on. The date
//! is the weaker validator an older client or an intermediary revalidates on when it kept no tag.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::http::{HeaderMap, header};

/// Does an `If-None-Match` field name the representation `etag` identifies?
///
/// RFC 9110 s13.1.2: `*` matches whenever a representation exists, a list matches when any member
/// does, and the comparison is weak, so `W/"x"` and `"x"` name the same bytes. A member that is not
/// an entity tag matches nothing, which leaves the full response a `304` would have replaced.
#[must_use]
pub fn if_none_match(field: &str, etag: &str) -> bool {
    field
        .split(',')
        .map(str::trim)
        .any(|candidate| candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == etag)
}

/// The `Range` to serve of the representation `etag` identifies, or `None` for the whole of it.
///
/// RFC 9110 s13.1.5: an `If-Range` carries the validator of the copy the client is resuming, and the
/// range is honored only while that validator still names the current representation. Anything else
/// drops the `Range` and serves the whole `200` the client would otherwise have to ask for a second
/// time: a weak tag says the bytes may differ. A `416` would be wrong here, since the request is well
/// formed and only the copy behind it is stale.
///
/// A date is refused as well, even though [`last_modified`] now gives these responses one to compare
/// against. The digest is the exact validator for content-addressed bytes, and a client that resumed
/// on a date it read here would gain nothing the tag does not already settle.
///
/// Only an exact strong match lets the range through, which is a byte comparison against a tag the
/// caller already holds: no allocation on the artifact hot path. An `If-Range` on a request without a
/// `Range` conditions nothing, so it is ignored.
///
/// A `Range` value is a range-set, so repeated field lines combine into a multi-range request this
/// server does not serve; it drops the `Range` and answers the whole representation, as one comma-joined
/// multi-range already does. An `If-Range` carries a single validator, not a list: a second field line
/// is malformed, so the range is not honored rather than trusting an arbitrary line of a contradictory
/// pair.
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

/// The date to serve for a representation the store wrote at `stored`, as of `now`.
///
/// An HTTP date counts whole seconds, so a sub-second timestamp is truncated rather than rounded up: a
/// date later than the write it stands for would refuse the very `If-Modified-Since` it taught the
/// client to send, and revalidation would never reach a `304`. A timestamp ahead of `now` - a clock
/// that stepped back, an mtime restored from an archive - is clamped for the same reason, since no
/// stored copy can predate the response that carried it.
#[must_use]
pub fn last_modified(stored: SystemTime, now: SystemTime) -> SystemTime {
    let seconds = stored.min(now).duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    UNIX_EPOCH + Duration::from_secs(seconds)
}

/// The `Last-Modified` field value for a date [`last_modified`] settled on.
#[must_use]
pub fn http_date(at: SystemTime) -> String {
    httpdate::fmt_http_date(at)
}

/// Does an `If-Modified-Since` field still cover a representation last modified at `modified`?
///
/// RFC 9110 s13.1.3: the condition holds - the client's copy is current, so a `304` - while the
/// representation is no newer than the date the request names. A field that is not a date states no
/// condition and is ignored, leaving the full response a `304` would have replaced; the three date
/// formats a recipient must accept all parse here, obsolete ones included, because the clients this
/// validator exists for are the old ones.
#[must_use]
pub fn if_modified_since(field: &str, modified: SystemTime) -> bool {
    httpdate::parse_http_date(field).is_ok_and(|since| modified <= since)
}
