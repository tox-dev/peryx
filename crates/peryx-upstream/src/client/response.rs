//! Simple-page response types and the header parsing that builds them.

use bytes::Bytes;
use futures_core::Stream;
use reqwest::StatusCode;
use reqwest::header::{CACHE_CONTROL, CONTENT_TYPE, ETAG, HeaderMap, HeaderName};
use url::Url;

use super::error::UpstreamError;

/// A response to an upstream simple-page fetch. Kept status-agnostic: `304` and `404` are returned
/// to the caller rather than raised, so the cache layer decides what to do.
#[derive(Debug, Clone)]
pub struct SimpleResponse {
    pub status: u16,
    /// The final URL fetched (after redirects), used as the base for resolving relative HTML links.
    pub url: Url,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub last_serial: Option<u64>,
    /// The freshness lifetime upstream granted via `Cache-Control`; `None` when the response
    /// carried no positive lifetime (absent header, `no-cache`, `no-store`, or zero).
    pub max_age: Option<i64>,
    pub body: Bytes,
}

/// The headers of a simple-page fetch with the body still open, for streaming.
#[derive(Debug)]
pub struct SimpleHead {
    pub status: u16,
    /// The final URL fetched (after redirects), the base for resolving relative HTML links.
    pub url: Url,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub last_serial: Option<u64>,
    /// The freshness lifetime upstream granted via `Cache-Control`; `None` when the response
    /// carried no positive lifetime (absent header, `no-cache`, `no-store`, or zero).
    pub max_age: Option<i64>,
    pub(super) response: reqwest::Response,
}

/// The parts of an artifact `HEAD` response needed before range reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileHead {
    pub len: u64,
}

impl SimpleHead {
    /// Read the whole body.
    ///
    /// # Errors
    /// Returns [`UpstreamError::Http`] if the transfer fails.
    pub async fn bytes(self) -> Result<Bytes, UpstreamError> {
        Ok(self.response.bytes().await?)
    }

    /// Consume the body as a stream of chunks.
    pub fn into_stream(self) -> impl Stream<Item = Result<Bytes, UpstreamError>> + Send + use<> {
        use futures_util::TryStreamExt as _;
        self.response.bytes_stream().map_err(UpstreamError::from)
    }
}

pub(super) fn header_str(headers: &HeaderMap, name: &HeaderName) -> Option<String> {
    headers.get(name)?.to_str().ok().map(str::to_owned)
}

pub(super) fn simple_head(response: reqwest::Response) -> Result<SimpleHead, UpstreamError> {
    let headers = response.headers();
    let content_type = header_str(headers, &CONTENT_TYPE);
    if response.status() == StatusCode::OK {
        validate_simple_content_type(response.url(), content_type.as_deref())?;
    }
    Ok(SimpleHead {
        status: response.status().as_u16(),
        url: response.url().clone(),
        content_type,
        etag: header_str(headers, &ETAG),
        last_serial: header_str(headers, &HeaderName::from_static("x-pypi-last-serial"))
            .and_then(|value| value.parse().ok()),
        max_age: max_age(headers),
        response,
    })
}

fn validate_simple_content_type(url: &Url, content_type: Option<&str>) -> Result<(), UpstreamError> {
    let Some(content_type) = content_type else {
        return Err(UpstreamError::MissingContentType { url: url.clone() });
    };
    let media_type = content_type
        .split_once(';')
        .map_or(content_type, |(media_type, _)| media_type)
        .trim()
        .to_ascii_lowercase();
    if matches!(
        media_type.as_str(),
        "application/vnd.pypi.simple.v1+json" | "application/vnd.pypi.simple.v1+html" | "text/html"
    ) {
        return Ok(());
    }
    Err(UpstreamError::UnsupportedContentType {
        url: url.clone(),
        content_type: content_type.to_owned(),
    })
}

/// The freshness lifetime a `Cache-Control` header grants a shared cache: `s-maxage` beats
/// `max-age`, `no-cache`/`no-store` disable caching, and a non-positive lifetime counts as none.
fn max_age(headers: &HeaderMap) -> Option<i64> {
    let value = headers.get(CACHE_CONTROL)?.to_str().ok()?;
    let mut max_age = None;
    let mut s_maxage = None;
    for directive in value.split(',') {
        let directive = directive.trim().to_ascii_lowercase();
        if directive == "no-cache" || directive == "no-store" {
            return None;
        }
        if let Some(secs) = directive.strip_prefix("max-age=").and_then(|v| v.parse().ok()) {
            max_age = Some(secs);
        }
        if let Some(secs) = directive.strip_prefix("s-maxage=").and_then(|v| v.parse().ok()) {
            s_maxage = Some(secs);
        }
    }
    s_maxage.or(max_age).filter(|&secs| secs > 0)
}
