//! `If-None-Match` against the entity tag of a content-addressed representation.
//!
//! The verified digest names an artifact's bytes, so it is the strong validator for them. A client
//! holding those bytes gets its answer from the request line, with no blob opened and no upstream
//! fetch started, which is why this sits next to the range grammar both blob servers lean on.

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
