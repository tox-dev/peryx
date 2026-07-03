//! The upstream HTTP client.

use bytes::Bytes;
use futures_core::Stream;
use reqwest::header::{ACCEPT, CACHE_CONTROL, CONTENT_TYPE, ETAG, HeaderMap, HeaderName, IF_NONE_MATCH};
use url::Url;

/// The `Accept` header velodex sends upstream: PEP 691 JSON first, then PEP 503 HTML.
const ACCEPT_SIMPLE: &str =
    "application/vnd.pypi.simple.v1+json, application/vnd.pypi.simple.v1+html;q=0.2, text/html;q=0.01";

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
    response: reqwest::Response,
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

/// An error talking to an upstream index.
#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    #[error(transparent)]
    Url(#[from] url::ParseError),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
}

/// How velodex authenticates to a private upstream. `Basic` covers pypi.org tokens (`__token__` +
/// token) and Artifactory/GitLab username/password; `Bearer` covers access/identity tokens.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Auth {
    #[default]
    None,
    Basic {
        username: String,
        password: String,
    },
    Bearer(String),
}

/// Redacted authentication shape for status surfaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStatus {
    None,
    Basic,
    Bearer,
}

impl AuthStatus {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Basic => "basic",
            Self::Bearer => "bearer",
        }
    }
}

/// A client for one upstream index, rooted at its `/simple/` base URL.
#[derive(Debug, Clone)]
pub struct UpstreamClient {
    http: reqwest::Client,
    /// File downloads only: HTTP/2 would multiplex every artifact over one TCP connection and its
    /// single congestion window, so bulk transfers force HTTP/1.1 and get a connection each.
    bulk: reqwest::Client,
    base: Url,
    auth: Auth,
}

impl UpstreamClient {
    /// Build an unauthenticated client for `base` (for example `https://pypi.org/simple/`).
    ///
    /// # Errors
    /// Returns [`UpstreamError::Url`] if `base` is not a valid URL, or [`UpstreamError::Http`] if
    /// the HTTP client cannot be built.
    pub fn new(base: &str) -> Result<Self, UpstreamError> {
        Self::with_auth(base, Auth::None)
    }

    /// Build a client for `base` with the given upstream authentication. A trailing slash is added
    /// if missing so project paths join correctly.
    ///
    /// # Errors
    /// Returns [`UpstreamError::Url`] if `base` is not a valid URL, or [`UpstreamError::Http`] if
    /// the HTTP client cannot be built.
    pub fn with_auth(base: &str, auth: Auth) -> Result<Self, UpstreamError> {
        // Pin the ring crypto provider: unlike aws-lc it is pure Rust plus portable assembly, so
        // every release target cross-compiles without a C toolchain. Err means already installed.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut base = Url::parse(base)?;
        if !base.path().ends_with('/') {
            let with_slash = format!("{}/", base.path());
            base.set_path(&with_slash);
        }
        let http = reqwest::Client::builder()
            .user_agent(concat!("velodex/", env!("CARGO_PKG_VERSION")))
            // Saturate the network: plenty of warm connections per upstream host, HTTP/2 with
            // adaptive flow-control windows, and no idle-pool eviction between resolver bursts.
            .pool_max_idle_per_host(32)
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .http2_adaptive_window(true)
            .tcp_keepalive(std::time::Duration::from_mins(1))
            .build()?;
        let bulk = reqwest::Client::builder()
            .user_agent(concat!("velodex/", env!("CARGO_PKG_VERSION")))
            .pool_max_idle_per_host(32)
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .http1_only()
            .tcp_keepalive(std::time::Duration::from_mins(1))
            .build()?;
        Ok(Self { http, bulk, base, auth })
    }

    fn authenticate(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.auth {
            Auth::None => request,
            Auth::Basic { username, password } => request.basic_auth(username, Some(password)),
            Auth::Bearer(token) => request.bearer_auth(token),
        }
    }

    /// Fetch a project's simple page, optionally revalidating with `If-None-Match`.
    ///
    /// # Errors
    /// Returns [`UpstreamError`] if the URL cannot be formed or the request fails.
    pub async fn fetch_project(&self, project: &str, etag: Option<&str>) -> Result<SimpleResponse, UpstreamError> {
        let response = self.head_project(project, etag).await?;
        let body = response.response.bytes().await?;
        Ok(SimpleResponse {
            status: response.status,
            url: response.url,
            content_type: response.content_type,
            etag: response.etag,
            last_serial: response.last_serial,
            max_age: response.max_age,
            body,
        })
    }

    /// Open a connection to the upstream host ahead of traffic, so the first real request skips
    /// the TCP and TLS handshakes. Failures are the first real request's problem to report.
    pub async fn warm(&self) {
        let _ = self.http.head(self.base.clone()).send().await;
    }

    /// Start fetching a project's simple page, returning its headers and the open body, so callers
    /// can stream the bytes as they arrive instead of buffering the page.
    ///
    /// # Errors
    /// Returns [`UpstreamError`] if the URL cannot be formed or the request fails.
    pub async fn head_project(&self, project: &str, etag: Option<&str>) -> Result<SimpleHead, UpstreamError> {
        let url = self.base.join(&format!("{project}/"))?;
        let mut request = self.authenticate(self.http.get(url)).header(ACCEPT, ACCEPT_SIMPLE);
        if let Some(etag) = etag {
            request = request.header(IF_NONE_MATCH, etag);
        }
        let response = request.send().await?;
        let headers = response.headers();
        Ok(SimpleHead {
            status: response.status().as_u16(),
            url: response.url().clone(),
            content_type: header_str(headers, &CONTENT_TYPE),
            etag: header_str(headers, &ETAG),
            last_serial: header_str(headers, &HeaderName::from_static("x-pypi-last-serial"))
                .and_then(|value| value.parse().ok()),
            max_age: max_age(headers),
            response,
        })
    }

    /// Start fetching a file's bytes from an absolute URL, for streaming.
    ///
    /// # Errors
    /// Returns [`UpstreamError::Http`] if the request fails or answers a non-success status.
    pub async fn stream_bytes(
        &self,
        url: &str,
    ) -> Result<impl Stream<Item = Result<Bytes, UpstreamError>> + Send + use<>, UpstreamError> {
        use futures_util::TryStreamExt as _;
        let response = self.authenticate(self.bulk.get(url)).send().await?.error_for_status()?;
        Ok(response.bytes_stream().map_err(UpstreamError::from))
    }

    /// Fetch a file's bytes from an absolute URL.
    ///
    /// # Errors
    /// Returns [`UpstreamError::Http`] if the request fails.
    pub async fn fetch_bytes(&self, url: &str) -> Result<Bytes, UpstreamError> {
        let response = self.authenticate(self.http.get(url)).send().await?;
        Ok(response.bytes().await?)
    }

    /// The upstream base URL with user info, query, and fragment removed for status pages.
    #[must_use]
    pub fn redacted_base_url(&self) -> String {
        let mut url = self.base.clone();
        let _ = url.set_username("");
        let _ = url.set_password(None);
        url.set_query(None);
        url.set_fragment(None);
        url.to_string()
    }

    /// The authentication scheme without credential material.
    #[must_use]
    pub const fn auth_status(&self) -> AuthStatus {
        match &self.auth {
            Auth::None => AuthStatus::None,
            Auth::Basic { .. } => AuthStatus::Basic,
            Auth::Bearer(_) => AuthStatus::Bearer,
        }
    }
}

fn header_str(headers: &HeaderMap, name: &HeaderName) -> Option<String> {
    headers.get(name)?.to_str().ok().map(str::to_owned)
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
