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

/// # Errors
/// Returns [`PathSafetyError::InvalidRoute`] for empty, traversal, encoded, or control-containing
/// routes, and [`PathSafetyError::ReservedRoute`] for supplied reserved prefixes.
pub fn validate_route<'a>(
    route: &str,
    reserved: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Result<(), PathSafetyError> {
    if route.is_empty() || route.starts_with('/') || route.ends_with('/') || route.contains("//") {
        return Err(PathSafetyError::InvalidRoute(route.to_owned()));
    }
    let (first, rest) = route
        .split_once('/')
        .map_or((route, None), |(first, rest)| (first, Some(rest)));
    if let Some((prefix, owner)) = reserved
        .into_iter()
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
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~'))
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
    Some(hex_nibble(high)? << 4 | hex_nibble(low)?)
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
