use velodex_storage::blob::Digest;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathSafetyError {
    #[error("invalid digest {0:?}: expected 64 lowercase hex sha256")]
    InvalidDigest(String),
    #[error(
        "invalid filename {0:?}: filenames must be relative path segments without separators, traversal, or control characters"
    )]
    InvalidFilename(String),
    #[error("invalid percent-encoded path segment {0:?}")]
    InvalidEncoding(String),
}

#[must_use]
pub fn local_file_url(route: &str, sha256: &str, filename: &str) -> String {
    format!("/{route}/files/{sha256}/{}", encode_path_segment(filename))
}

#[must_use]
pub fn encode_path_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(byte as char),
            other => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{other:02X}");
            }
        }
    }
    out
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
        PathSafetyError, decode_path, decode_path_segment, encode_path_segment, local_file_url, parse_digest,
        validate_filename,
    };

    #[test]
    fn test_path_segments_encode_reserved_characters() {
        assert_eq!(
            local_file_url("root/pypi", "aa", "pkg 1.0#x?.whl"),
            "/root/pypi/files/aa/pkg%201.0%23x%3F.whl"
        );
        assert_eq!(encode_path_segment("pkg/name.whl"), "pkg%2Fname.whl");
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
}
