use velodex_core::url_encoding::{push_component, push_path};
use velodex_storage::blob::Digest;

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
    #[error("invalid route {0:?}: the first route segment is reserved by velodex")]
    ReservedRoute(String),
    #[error("invalid percent-encoded path segment {0:?}")]
    InvalidEncoding(String),
}

const RESERVED_ROUTE_PREFIXES: &[&str] = &[
    "+stats", "+status", "admin", "api-docs", "browse", "metrics", "pkg", "stats",
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

/// Decode a percent-encoded route segment.
///
/// # Errors
/// Returns [`PathSafetyError::InvalidEncoding`] if the segment contains malformed percent escapes
/// or decodes to non-UTF-8 bytes.
pub fn decode_path_segment(segment: &str) -> Result<String, PathSafetyError> {
    decode_percent(segment)
}

/// Decode a percent-encoded path remainder.
///
/// # Errors
/// Returns [`PathSafetyError::InvalidEncoding`] if the path contains malformed percent escapes or
/// decodes to non-UTF-8 bytes.
pub fn decode_path(path: &str) -> Result<String, PathSafetyError> {
    decode_percent(path)
}

/// Parse a sha256 digest from a route parameter.
///
/// # Errors
/// Returns [`PathSafetyError::InvalidDigest`] if `hex` is not exactly 64 lowercase hex characters.
pub fn parse_digest(hex: &str) -> Result<Digest, PathSafetyError> {
    Digest::from_hex(hex).ok_or_else(|| PathSafetyError::InvalidDigest(hex.to_owned()))
}

/// Validate an index route as a raw URL path prefix.
///
/// # Errors
/// Returns [`PathSafetyError::InvalidRoute`] for empty, traversal, encoded, or control-containing
/// routes, and [`PathSafetyError::ReservedRoute`] for prefixes owned by Velodex itself.
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

fn decode_percent(input: &str) -> Result<String, PathSafetyError> {
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
    String::from_utf8(out).map_err(|_| PathSafetyError::InvalidEncoding(input.to_owned()))
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
mod tests {
    use super::{
        PathSafetyError, decode_path, decode_path_segment, local_file_url, parse_digest, validate_filename,
        validate_path_segment, validate_route,
    };

    #[test]
    fn test_path_segments_encode_reserved_characters() {
        assert_eq!(
            local_file_url("root/pypi", "aa", "pkg 1.0#x?.whl"),
            "/root/pypi/files/aa/pkg%201.0%23x%3F.whl"
        );
    }

    #[test]
    fn test_path_segments_decode_percent_encoding() {
        assert_eq!(decode_path_segment("pkg%201.0%23x%3F.whl").unwrap(), "pkg 1.0#x?.whl");
        assert_eq!(decode_path_segment("pkg%252Fname.whl").unwrap(), "pkg%2Fname.whl");
        assert_eq!(
            decode_path_segment("pkg%2"),
            Err(PathSafetyError::InvalidEncoding("pkg%2".to_owned()))
        );
        assert_eq!(
            decode_path_segment("pkg%xx"),
            Err(PathSafetyError::InvalidEncoding("pkg%xx".to_owned()))
        );
        assert_eq!(
            decode_path_segment("pkg%0x"),
            Err(PathSafetyError::InvalidEncoding("pkg%0x".to_owned()))
        );
        assert_eq!(
            decode_path_segment("pkg%ff"),
            Err(PathSafetyError::InvalidEncoding("pkg%ff".to_owned()))
        );
    }

    #[test]
    fn test_paths_decode_member_separators() {
        assert_eq!(
            decode_path("velodexpkg-1.0.dist-info%2FMETADATA").unwrap(),
            "velodexpkg-1.0.dist-info/METADATA"
        );
    }

    #[test]
    fn test_digest_parser_explains_shape() {
        assert_eq!(
            parse_digest("nothex").unwrap_err(),
            PathSafetyError::InvalidDigest("nothex".to_owned())
        );
    }

    #[test]
    fn test_route_validation_accepts_nested_unreserved_routes() {
        assert_eq!(validate_route("root/pypi-1.0_~"), Ok(()));
    }

    #[test]
    fn test_route_validation_rejects_unsafe_or_reserved_routes() {
        for route in [
            "",
            "/pypi",
            "pypi/",
            "root//pypi",
            ".",
            "root/..",
            "root/pypi mirror",
            "root/%70ypi",
        ] {
            assert_eq!(
                validate_route(route),
                Err(PathSafetyError::InvalidRoute(route.to_owned()))
            );
        }
        assert_eq!(
            validate_route("browse/private"),
            Err(PathSafetyError::ReservedRoute("browse/private".to_owned()))
        );
        assert_eq!(
            validate_route("admin/status"),
            Err(PathSafetyError::ReservedRoute("admin/status".to_owned()))
        );
    }

    #[test]
    fn test_filename_validation_rejects_path_inputs() {
        for filename in [
            "",
            ".",
            "..",
            "../pkg.whl",
            "pkg/name.whl",
            "pkg\\name.whl",
            "pkg\u{7}.whl",
        ] {
            assert!(validate_filename(filename).is_err(), "{filename:?}");
        }
        assert!(validate_filename("pkg 1.0#x?.whl").is_ok());
    }

    #[test]
    fn test_path_segment_validation_rejects_decoded_separators() {
        assert_eq!(validate_path_segment("version", "1.0+local"), Ok(()));
        assert_eq!(
            validate_path_segment("version", "1.0/local"),
            Err(PathSafetyError::InvalidPathSegment {
                kind: "version",
                value: "1.0/local".to_owned()
            })
        );
    }
}
