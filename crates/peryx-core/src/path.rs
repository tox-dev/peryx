use crate::url_encoding::{push_component, push_path};
use std::borrow::Cow;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathSafetyError {
    #[error("invalid digest {0:?}: expected 64 lowercase hex sha256")]
    InvalidDigest(String),
    #[error(
        "invalid artifact name {0:?}: artifact names must be relative path segments without separators, traversal, or control characters"
    )]
    InvalidArtifactName(String),
    #[error(
        "invalid {kind} {value:?}: path parameters must be non-empty segments without separators, traversal, or control characters"
    )]
    InvalidPathSegment { kind: &'static str, value: String },
    #[error("invalid route {0:?}: routes must be non-empty unreserved path segments separated by '/'")]
    InvalidRoute(String),
    #[error("invalid route {route:?}: prefix {prefix:?} is reserved by {owner}")]
    ReservedRoute {
        route: String,
        prefix: String,
        owner: String,
    },
    #[error("invalid percent-encoded path segment {0:?}")]
    InvalidEncoding(String),
}

/// Reserved prefixes prevent index routes from shadowing Peryx endpoints.
pub const CORE_ROUTE_PREFIXES: &[&str] = &["_", "api-docs", "favicon.svg", "metrics", "pkg"];

#[must_use]
pub fn local_artifact_url(route: &str, sha256: &str, artifact: &str) -> String {
    let mut url = String::with_capacity(route.len() + sha256.len() + artifact.len() + 9);
    url.push('/');
    push_path(&mut url, route);
    url.push_str("/files/");
    url.push_str(sha256);
    url.push('/');
    push_component(&mut url, artifact);
    url
}

/// Whether `url` is the complete local URL for this artifact.
#[must_use]
pub fn is_local_artifact_url(route: &str, sha256: &str, artifact: &str, url: &str) -> bool {
    local_artifact_url(route, sha256, artifact) == url
}

/// # Errors
/// Returns [`PathSafetyError::InvalidEncoding`] if the segment contains malformed percent escapes
/// or decodes to non-UTF-8 bytes.
pub fn decode_path_segment(segment: &str) -> Result<Cow<'_, str>, PathSafetyError> {
    decode_percent(segment)
}

/// # Errors
/// Returns [`PathSafetyError::InvalidEncoding`] if the path contains malformed percent escapes or
/// decodes to non-UTF-8 bytes.
pub fn decode_path(path: &str) -> Result<Cow<'_, str>, PathSafetyError> {
    decode_percent(path)
}

/// The equivalent spelling of `path` that peryx itself would emit.
///
/// A percent-escaped unreserved octet denotes the same character as the octet itself (RFC 3986
/// §6.2.2.2), so unescaping one can only name something the client could have spelled literally.
/// Reserved and excluded octets keep their escapes, so segment boundaries and every value a
/// consumer later decodes are left exactly as they arrived. Malformed escapes are left alone for
/// the consumer that decodes them to reject.
#[must_use]
pub fn canonicalize_path(path: &str) -> Cow<'_, str> {
    if !path.contains('%') {
        return Cow::Borrowed(path);
    }
    let mut canonical = String::with_capacity(path.len());
    let mut rest = path;
    while let Some(escape) = rest.find('%') {
        canonical.push_str(&rest[..escape]);
        let escape = &rest[escape..];
        let unreserved = escape
            .as_bytes()
            .get(1..3)
            .and_then(|digits| hex_byte(digits[0], digits[1]))
            .filter(|byte| is_unreserved(*byte));
        if let Some(byte) = unreserved {
            canonical.push(char::from(byte));
            rest = &escape[3..];
        } else {
            canonical.push('%');
            rest = &escape[1..];
        }
    }
    canonical.push_str(rest);
    Cow::Owned(canonical)
}

/// # Errors
/// Returns [`PathSafetyError::InvalidRoute`] for empty, traversal, encoded, or control-containing
/// routes, and [`PathSafetyError::ReservedRoute`] for supplied reserved prefixes.
pub fn validate_route(route: &str, reserved: &[(&str, &str)]) -> Result<(), PathSafetyError> {
    if route.is_empty() || route.starts_with('/') || route.ends_with('/') || route.contains("//") {
        return Err(PathSafetyError::InvalidRoute(route.to_owned()));
    }
    let (first, rest) = route
        .split_once('/')
        .map_or((route, None), |(first, rest)| (first, Some(rest)));
    if let Some((prefix, owner)) = reserved
        .iter()
        .copied()
        .find(|(prefix, _)| prefix.trim_matches('/').split('/').next() == Some(first))
    {
        return Err(PathSafetyError::ReservedRoute {
            route: route.to_owned(),
            prefix: prefix.to_owned(),
            owner: owner.to_owned(),
        });
    }
    if !valid_route_segment(first)
        || rest.is_some_and(|rest| rest.split('/').any(|segment| !valid_route_segment(segment)))
    {
        return Err(PathSafetyError::InvalidRoute(route.to_owned()));
    }
    Ok(())
}

/// # Errors
/// Returns [`PathSafetyError::InvalidArtifactName`] for empty names, traversal names, separators, or
/// control characters.
pub fn validate_artifact_name(artifact: &str) -> Result<(), PathSafetyError> {
    if artifact.is_empty()
        || artifact == "."
        || artifact == ".."
        || artifact.contains('/')
        || artifact.contains('\\')
        || artifact.chars().any(char::is_control)
    {
        Err(PathSafetyError::InvalidArtifactName(artifact.to_owned()))
    } else {
        Ok(())
    }
}

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
    !segment.is_empty() && segment != "." && segment != ".." && segment.bytes().all(is_unreserved)
}

const fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

/// Borrow unescaped input to avoid an allocation on the common path.
fn decode_percent(input: &str) -> Result<Cow<'_, str>, PathSafetyError> {
    if !input.contains('%') {
        return Ok(Cow::Borrowed(input));
    }
    let invalid_encoding = || PathSafetyError::InvalidEncoding(input.to_owned());
    let mut out = Vec::with_capacity(input.len());
    let mut bytes = input.bytes();
    while let Some(byte) = bytes.next() {
        if byte != b'%' {
            out.push(byte);
            continue;
        }
        let (Some(high), Some(low)) = (bytes.next(), bytes.next()) else {
            return Err(invalid_encoding());
        };
        let Some(byte) = hex_byte(high, low) else {
            return Err(invalid_encoding());
        };
        out.push(byte);
    }
    String::from_utf8(out).map(Cow::Owned).map_err(|_| invalid_encoding())
}

fn hex_byte(high: u8, low: u8) -> Option<u8> {
    let digits = [high, low];
    u8::from_str_radix(std::str::from_utf8(&digits).ok()?, 16).ok()
}

#[cfg(test)]
#[path = "../tests/unit/path/tests.rs"]
mod tests;
