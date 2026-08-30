use reqwest::header::{ETAG, HeaderMap, HeaderName, HeaderValue};

pub(super) fn header_str(headers: &HeaderMap, name: &HeaderName) -> Option<String> {
    headers.get(name)?.to_str().ok().map(str::to_owned)
}

/// Returns the entity tag only when it is strong, because `If-Range` accepts nothing weaker as a
/// representation boundary (RFC 9110 section 13.1.5).
pub(super) fn strong_etag(headers: &HeaderMap) -> Option<HeaderValue> {
    let etag = headers.get(ETAG)?;
    let value = etag.as_bytes();
    let inner = value.strip_prefix(b"\"")?.strip_suffix(b"\"")?;
    inner
        .iter()
        .all(|byte| matches!(byte, 0x21 | 0x23..=0x7e | 0x80..=0xff))
        .then(|| etag.clone())
}
