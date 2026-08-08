//! Guarding the parts of a URL path peryx builds and parses: index routes, artifact filenames,
//! and percent-encoded path segments.
//!
//! Pure string work, so it sits in the core beneath every crate that constructs or validates a
//! path: the serving layer, the ecosystem drivers, and the binary's config validation alike.

use crate::url_encoding::{push_component, push_path};
use std::borrow::Cow;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathSafetyError {
    #[error("invalid digest {0:?}: expected 64 lowercase hex sha256")]
    InvalidDigest(String),
    #[error(
        "invalid filename {0:?}: filenames must be relative path segments without separators, traversal, or control characters"
    )]
    InvalidFilename(String),
    #[error(
        "invalid {kind} {value:?}: path parameters must be non-empty segments without separators, traversal, or control characters"
    )]
    InvalidPathSegment { kind: &'static str, value: String },
    #[error("invalid route {0:?}: routes must be non-empty unreserved path segments separated by '/'")]
    InvalidRoute(String),
    #[error("invalid route {0:?}: the first route segment is reserved by peryx")]
    ReservedRoute(String),
    #[error("invalid percent-encoded path segment {0:?}")]
    InvalidEncoding(String),
}

/// Top-level path prefixes peryx's own router owns - the neutral API surface, the web UI's pages, and
/// its static assets - that an index route would otherwise be shadowed by. Kept in step with the axum
/// router and the web UI's page table; a served path missing here lets an index route resolve to a
/// built-in page instead. `_` is the machine-endpoint namespace (the `/_/oidc/*` trusted-publishing
/// routes): a valid route segment, so a route claiming it would shadow those endpoints.
const RESERVED_ROUTE_PREFIXES: &[&str] = &[
    "+stats",
    "+status",
    "_",
    "admin",
    "api-docs",
    "browse",
    "favicon.svg",
    "metrics",
    "pkg",
    "search",
    "stats",
    "upload",
];

#[must_use]
pub fn local_file_url(route: &str, sha256: &str, filename: &str) -> String {
    let mut url = String::with_capacity(route.len() + sha256.len() + filename.len() + 9);
    url.push('/');
    push_path(&mut url, route);
    url.push_str("/files/");
    url.push_str(sha256);
    url.push('/');
    push_component(&mut url, filename);
    url
}

/// Whether `url` is a peryx-local file URL on `route`, the shape [`local_file_url`] produces.
///
/// This is the marker for an already-rewritten cache record. A bare leading `/` is not enough: a
/// An upstream may serve a legitimate root-relative artifact URL (`/artifacts/x.bin`), which must
/// still resolve to a real blob rather than be mistaken for a local record.
#[must_use]
pub fn is_local_file_url(route: &str, url: &str) -> bool {
    let mut prefix = String::with_capacity(route.len() + 8);
    prefix.push('/');
    push_path(&mut prefix, route);
    prefix.push_str("/files/");
    url.starts_with(&prefix)
}

/// Decode a percent-encoded route segment.
///
/// # Errors
/// Returns [`PathSafetyError::InvalidEncoding`] if the segment contains malformed percent escapes
/// or decodes to non-UTF-8 bytes.
pub fn decode_path_segment(segment: &str) -> Result<Cow<'_, str>, PathSafetyError> {
    decode_percent(segment)
}

/// Decode a percent-encoded path remainder.
///
/// # Errors
/// Returns [`PathSafetyError::InvalidEncoding`] if the path contains malformed percent escapes or
/// decodes to non-UTF-8 bytes.
pub fn decode_path(path: &str) -> Result<Cow<'_, str>, PathSafetyError> {
    decode_percent(path)
}

/// Validate an index route as a raw URL path prefix.
///
/// # Errors
/// Returns [`PathSafetyError::InvalidRoute`] for empty, traversal, encoded, or control-containing
/// routes, and [`PathSafetyError::ReservedRoute`] for prefixes owned by Peryx itself.
pub fn validate_route(route: &str) -> Result<(), PathSafetyError> {
    if route.is_empty() || route.starts_with('/') || route.ends_with('/') || route.contains("//") {
        return Err(PathSafetyError::InvalidRoute(route.to_owned()));
    }
    let (first, rest) = route
        .split_once('/')
        .map_or((route, None), |(first, rest)| (first, Some(rest)));
    if RESERVED_ROUTE_PREFIXES.contains(&first) {
        return Err(PathSafetyError::ReservedRoute(route.to_owned()));
    }
    if !valid_route_segment(first)
        || rest.is_some_and(|rest| rest.split('/').any(|segment| !valid_route_segment(segment)))
    {
        return Err(PathSafetyError::InvalidRoute(route.to_owned()));
    }
    Ok(())
}

/// Validate a display filename as one safe path segment.
///
/// # Errors
/// Returns [`PathSafetyError::InvalidFilename`] for empty names, traversal names, separators, or
/// control characters.
pub fn validate_filename(filename: &str) -> Result<(), PathSafetyError> {
    if filename.is_empty()
        || filename == "."
        || filename == ".."
        || filename.contains('/')
        || filename.contains('\\')
        || filename.chars().any(char::is_control)
    {
        Err(PathSafetyError::InvalidFilename(filename.to_owned()))
    } else {
        Ok(())
    }
}

/// Validate a decoded route parameter as one path segment.
///
/// # Errors
/// Returns [`PathSafetyError::InvalidPathSegment`] for empty values, traversal segments,
/// separators, or control characters.
pub fn validate_path_segment(kind: &'static str, value: &str) -> Result<(), PathSafetyError> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
        || value.chars().any(char::is_control)
    {
        Err(PathSafetyError::InvalidPathSegment {
            kind,
            value: value.to_owned(),
        })
    } else {
        Ok(())
    }
}

fn valid_route_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~'))
}

fn hex_byte(hex: &[u8]) -> Option<u8> {
    Some(hex_nibble(hex[0])? << 4 | hex_nibble(hex[1])?)
}

/// Percent-decode, borrowing when there is nothing to decode.
///
/// An escape starts with `%`, and artifact coordinates seldom carry
/// one. Copying every byte through a fresh buffer to discover that cost an allocation per segment on
/// every request.
fn decode_percent(input: &str) -> Result<Cow<'_, str>, PathSafetyError> {
    if !input.contains('%') {
        return Ok(Cow::Borrowed(input));
    }
    let mut out = Vec::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut position = 0;
    while position < bytes.len() {
        if bytes[position] == b'%' {
            let Some(hex) = bytes.get(position + 1..position + 3) else {
                return Err(PathSafetyError::InvalidEncoding(input.to_owned()));
            };
            let Some(byte) = hex_byte(hex) else {
                return Err(PathSafetyError::InvalidEncoding(input.to_owned()));
            };
            out.push(byte);
            position += 3;
        } else {
            out.push(bytes[position]);
            position += 1;
        }
    }
    String::from_utf8(out)
        .map(Cow::Owned)
        .map_err(|_| PathSafetyError::InvalidEncoding(input.to_owned()))
}

const fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
#[path = "../tests/unit/path/tests.rs"]
mod tests;
