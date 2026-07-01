//! The upstream HTTP client.

use bytes::Bytes;
use reqwest::header::{ACCEPT, CONTENT_TYPE, ETAG, HeaderMap, HeaderName, IF_NONE_MATCH};
use url::Url;

/// The `Accept` header velox sends upstream: PEP 691 JSON first, then PEP 503 HTML.
const ACCEPT_SIMPLE: &str =
    "application/vnd.pypi.simple.v1+json, application/vnd.pypi.simple.v1+html;q=0.2, text/html;q=0.01";

/// A response to an upstream simple-page fetch. Kept status-agnostic: `304` and `404` are returned
/// to the caller rather than raised, so the cache layer decides what to do.
#[derive(Debug, Clone)]
pub struct SimpleResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub last_serial: Option<u64>,
    pub body: Bytes,
}

/// An error talking to an upstream index.
#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    #[error(transparent)]
    Url(#[from] url::ParseError),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
}

/// A client for one upstream index, rooted at its `/simple/` base URL.
#[derive(Debug, Clone)]
pub struct UpstreamClient {
    http: reqwest::Client,
    base: Url,
}

impl UpstreamClient {
    /// Build a client for `base` (for example `https://pypi.org/simple/`). A trailing slash is
    /// added if missing so project paths join correctly.
    ///
    /// # Errors
    /// Returns [`UpstreamError::Url`] if `base` is not a valid URL, or [`UpstreamError::Http`] if
    /// the HTTP client cannot be built.
    pub fn new(base: &str) -> Result<Self, UpstreamError> {
        let mut base = Url::parse(base)?;
        if !base.path().ends_with('/') {
            let with_slash = format!("{}/", base.path());
            base.set_path(&with_slash);
        }
        let http = reqwest::Client::builder()
            .user_agent(concat!("velox/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { http, base })
    }

    /// Fetch a project's simple page, optionally revalidating with `If-None-Match`.
    ///
    /// # Errors
    /// Returns [`UpstreamError`] if the URL cannot be formed or the request fails.
    pub async fn fetch_project(&self, project: &str, etag: Option<&str>) -> Result<SimpleResponse, UpstreamError> {
        let url = self.base.join(&format!("{project}/"))?;
        let mut request = self.http.get(url).header(ACCEPT, ACCEPT_SIMPLE);
        if let Some(etag) = etag {
            request = request.header(IF_NONE_MATCH, etag);
        }
        into_simple(request.send().await?).await
    }

    /// Fetch a file's bytes from an absolute URL.
    ///
    /// # Errors
    /// Returns [`UpstreamError::Http`] if the request fails.
    pub async fn fetch_bytes(&self, url: &str) -> Result<Bytes, UpstreamError> {
        let response = self.http.get(url).send().await?;
        Ok(response.bytes().await?)
    }
}

async fn into_simple(response: reqwest::Response) -> Result<SimpleResponse, UpstreamError> {
    let status = response.status().as_u16();
    let headers = response.headers();
    let content_type = header_str(headers, &CONTENT_TYPE);
    let etag = header_str(headers, &ETAG);
    let last_serial = header_str(headers, &HeaderName::from_static("x-pypi-last-serial")).and_then(|v| v.parse().ok());
    let body = response.bytes().await?;
    Ok(SimpleResponse {
        status,
        content_type,
        etag,
        last_serial,
        body,
    })
}

fn header_str(headers: &HeaderMap, name: &HeaderName) -> Option<String> {
    headers.get(name)?.to_str().ok().map(str::to_owned)
}
