//! The upstream HTTP client.

mod error;
mod response;
mod retry;

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures_core::Stream;
use reqwest::header::{
    ACCEPT, ACCEPT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, HeaderMap, HeaderName, IF_NONE_MATCH, RANGE,
};
use url::Url;

use self::response::{header_str, simple_head};
use self::retry::{
    MAX_RETRIES, should_retry_error, should_retry_status, sleep_before_retry, sleep_before_retry_status,
    sleep_before_retry_str,
};

pub use self::error::{RangeError, UpstreamError};
pub use self::response::{FileHead, SimpleHead, SimpleResponse};

/// The `Accept` header peryx sends upstream: PEP 691 JSON first, then PEP 503 HTML.
const ACCEPT_SIMPLE: &str =
    "application/vnd.pypi.simple.v1+json, application/vnd.pypi.simple.v1+html;q=0.2, text/html;q=0.01";
const USER_AGENT: &str = concat!("peryx/", env!("CARGO_PKG_VERSION"));
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How peryx authenticates to a private upstream. `Basic` covers pypi.org tokens (`__token__` +
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
    range_support: Arc<AtomicU8>,
}

const RANGE_UNKNOWN: u8 = 0;
const RANGE_SUPPORTED: u8 = 1;
const RANGE_UNSUPPORTED: u8 = 2;

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
            .user_agent(USER_AGENT)
            // Saturate the network: plenty of warm connections per upstream host, HTTP/2 with
            // adaptive flow-control windows, and no idle-pool eviction between resolver bursts.
            .pool_max_idle_per_host(32)
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .http2_adaptive_window(true)
            .tcp_keepalive(std::time::Duration::from_mins(1))
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()?;
        let bulk = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .pool_max_idle_per_host(32)
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .http1_only()
            .tcp_keepalive(std::time::Duration::from_mins(1))
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()?;
        Ok(Self {
            http,
            bulk,
            base,
            auth,
            range_support: Arc::new(AtomicU8::new(RANGE_UNKNOWN)),
        })
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
        let url = self.base.join(&format!("{project}/"))?;
        self.fetch_simple(url, etag).await
    }

    /// Fetch the upstream root project list.
    ///
    /// # Errors
    /// Returns [`UpstreamError`] if the request fails.
    pub async fn fetch_index(&self) -> Result<SimpleResponse, UpstreamError> {
        self.fetch_simple(self.base.clone(), None).await
    }

    async fn fetch_simple(&self, url: Url, etag: Option<&str>) -> Result<SimpleResponse, UpstreamError> {
        let mut attempt = 0;
        loop {
            let response = self.send_simple(&url, etag).await?;
            let head = simple_head(response)?;
            match head.response.bytes().await {
                Ok(body) => {
                    return Ok(SimpleResponse {
                        status: head.status,
                        url: head.url,
                        content_type: head.content_type,
                        etag: head.etag,
                        last_serial: head.last_serial,
                        max_age: head.max_age,
                        body,
                    });
                }
                Err(err) if should_retry_error(&err) && attempt < MAX_RETRIES => {
                    sleep_before_retry(&head.url, attempt, &err).await;
                    attempt += 1;
                }
                Err(err) => return Err(err.into()),
            }
        }
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
        simple_head(self.send_simple(&url, etag).await?)
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
        let response = self
            .send_with_retry(|| self.authenticate(self.bulk.get(url).header(ACCEPT_ENCODING, "identity")))
            .await?
            .error_for_status()?;
        Ok(response.bytes_stream().map_err(UpstreamError::from))
    }

    /// Fetch a file's bytes from an absolute URL.
    ///
    /// # Errors
    /// Returns [`UpstreamError::Http`] if the request fails or answers a non-success status.
    pub async fn fetch_bytes(&self, url: &str) -> Result<Bytes, UpstreamError> {
        let mut attempt = 0;
        loop {
            let response = self
                .send_with_retry(|| self.authenticate(self.bulk.get(url).header(ACCEPT_ENCODING, "identity")))
                .await?
                .error_for_status()?;
            match response.bytes().await {
                Ok(bytes) => return Ok(bytes),
                Err(err) if should_retry_error(&err) && attempt < MAX_RETRIES => {
                    sleep_before_retry_str(url, attempt, &err).await;
                    attempt += 1;
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    /// Whether this index should still try byte range reads.
    #[must_use]
    pub fn may_support_ranges(&self) -> bool {
        self.range_support.load(Ordering::Relaxed) != RANGE_UNSUPPORTED
    }

    /// Stop trying byte range reads for this index during this process.
    pub fn disable_ranges(&self) {
        self.range_support.store(RANGE_UNSUPPORTED, Ordering::Relaxed);
    }

    /// Fetch artifact headers for a future range read.
    ///
    /// # Errors
    /// Returns [`RangeError::Unsupported`] when upstream omits range support or length metadata,
    /// and [`RangeError::Upstream`] on other request failures.
    pub async fn head_file_for_range(&self, url: &str) -> Result<FileHead, RangeError> {
        let response = self
            .authenticate(self.http.head(url))
            .header(ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(UpstreamError::from)?;
        if head_status_disables_ranges(response.status()) {
            self.disable_ranges();
            return Err(RangeError::Unsupported);
        }
        let response = response.error_for_status().map_err(UpstreamError::from)?;
        let headers = response.headers();
        if !headers
            .get(HeaderName::from_static("accept-ranges"))
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.split(',').any(|part| part.trim().eq_ignore_ascii_case("bytes")))
        {
            self.disable_ranges();
            return Err(RangeError::Unsupported);
        }
        let Some(len) = header_str(headers, &CONTENT_LENGTH).and_then(|value| value.parse().ok()) else {
            self.disable_ranges();
            return Err(RangeError::Unsupported);
        };
        self.range_support.store(RANGE_SUPPORTED, Ordering::Relaxed);
        Ok(FileHead { len })
    }

    /// Fetch an inclusive byte range from an artifact URL.
    ///
    /// # Errors
    /// Returns [`RangeError::Unsupported`] or [`RangeError::Invalid`] when upstream cannot satisfy
    /// the requested range, and [`RangeError::Upstream`] on other request failures.
    pub async fn fetch_range(&self, url: &str, start: u64, end: u64) -> Result<Bytes, RangeError> {
        if end < start {
            return Err(RangeError::Invalid(format!("start {start} is after end {end}")));
        }
        let Some(range_len) = (end - start).checked_add(1) else {
            return Err(RangeError::Invalid("requested range length overflowed".to_owned()));
        };
        #[cfg(target_pointer_width = "64")]
        let expected_len = usize::try_from(range_len).unwrap_or(usize::MAX);
        #[cfg(not(target_pointer_width = "64"))]
        let Ok(expected_len) = usize::try_from(range_len) else {
            return Err(RangeError::Invalid("requested range does not fit memory".to_owned()));
        };
        let response = self
            .authenticate(self.http.get(url))
            .header(ACCEPT_ENCODING, "identity")
            .header(RANGE, format!("bytes={start}-{end}"))
            .send()
            .await
            .map_err(UpstreamError::from)?;
        match response.status() {
            reqwest::StatusCode::PARTIAL_CONTENT => {}
            reqwest::StatusCode::OK | reqwest::StatusCode::RANGE_NOT_SATISFIABLE => {
                self.disable_ranges();
                return Err(RangeError::Unsupported);
            }
            _ => {
                response.error_for_status().map_err(UpstreamError::from)?;
                return Err(RangeError::Invalid(
                    "range request returned a non-206 success".to_owned(),
                ));
            }
        }
        if let Err(err) = validate_content_range(response.headers(), start, end) {
            self.disable_ranges();
            return Err(err);
        }
        let bytes = response.bytes().await.map_err(UpstreamError::from)?;
        if bytes.len() != expected_len {
            self.disable_ranges();
            return Err(RangeError::Invalid(format!(
                "expected {expected_len} bytes, received {}",
                bytes.len()
            )));
        }
        self.range_support.store(RANGE_SUPPORTED, Ordering::Relaxed);
        Ok(bytes)
    }

    /// The upstream base URL with user info, query, and fragment removed for status pages.
    #[must_use]
    pub fn redacted_base_url(&self) -> String {
        redact_url(self.base.as_ref())
    }

    /// The configured upstream base URL, trailing slash included. Carries credential material if the
    /// configured URL did, so callers that surface it to users must redact first.
    #[must_use]
    pub fn base_url(&self) -> &str {
        self.base.as_str()
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

    /// The configured credentials, carrying secret material. A protocol that authenticates outside the
    /// simple request path (the OCI bearer-token exchange trades Basic credentials at a realm for a
    /// scoped token) reads them here; anything user-facing must go through [`Self::auth_status`] instead.
    #[must_use]
    pub const fn auth(&self) -> &Auth {
        &self.auth
    }

    async fn send_simple(&self, url: &Url, etag: Option<&str>) -> Result<reqwest::Response, UpstreamError> {
        self.send_with_retry(|| {
            let mut request = self
                .authenticate(self.http.get(url.clone()))
                .header(ACCEPT, ACCEPT_SIMPLE);
            if let Some(etag) = etag {
                request = request.header(IF_NONE_MATCH, etag);
            }
            request
        })
        .await
    }

    async fn send_with_retry(
        &self,
        mut request: impl FnMut() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, UpstreamError> {
        let mut attempt = 0;
        loop {
            match request().send().await {
                Ok(response) if should_retry_status(response.status()) && attempt < MAX_RETRIES => {
                    let url = response.url().clone();
                    let status = response.status();
                    sleep_before_retry_status(&url, attempt, status).await;
                    attempt += 1;
                }
                Ok(response) => return Ok(response),
                Err(err) if should_retry_error(&err) && attempt < MAX_RETRIES => {
                    sleep_before_retry_str(err.url().map_or("unknown URL", Url::as_str), attempt, &err).await;
                    attempt += 1;
                }
                Err(err) => return Err(err.into()),
            }
        }
    }
}

/// Remove credential-bearing URL parts before displaying configured upstreams.
#[must_use]
pub fn redact_url(value: &str) -> String {
    let Ok(mut url) = Url::parse(value) else {
        return "<invalid upstream URL>".to_owned();
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    url.to_string()
}

const fn head_status_disables_ranges(status: reqwest::StatusCode) -> bool {
    matches!(
        status,
        reqwest::StatusCode::BAD_REQUEST
            | reqwest::StatusCode::FORBIDDEN
            | reqwest::StatusCode::NOT_FOUND
            | reqwest::StatusCode::METHOD_NOT_ALLOWED
            | reqwest::StatusCode::NOT_IMPLEMENTED
    )
}

fn validate_content_range(headers: &HeaderMap, start: u64, end: u64) -> Result<(), RangeError> {
    let value = headers
        .get(CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| RangeError::Invalid("missing Content-Range".to_owned()))?;
    let Some(rest) = value.strip_prefix("bytes ") else {
        return Err(RangeError::Invalid(format!("unexpected Content-Range {value:?}")));
    };
    let Some((actual, _total)) = rest.split_once('/') else {
        return Err(RangeError::Invalid(format!("unexpected Content-Range {value:?}")));
    };
    let Some((actual_start, actual_end)) = actual.split_once('-') else {
        return Err(RangeError::Invalid(format!("unexpected Content-Range {value:?}")));
    };
    if actual_start.parse::<u64>().ok() == Some(start) && actual_end.parse::<u64>().ok() == Some(end) {
        Ok(())
    } else {
        Err(RangeError::Invalid(format!(
            "expected Content-Range bytes {start}-{end}, got {value:?}"
        )))
    }
}

/// The upstream fetch protocol a proxy index speaks.
///
/// A proxy revalidates and caches an upstream's index documents and files. This trait is the seam
/// that logic plugs into: [`UpstreamClient`] implements the `PyPI` PEP 503/691 simple API, and an OCI
/// registry (`/v2/`) or an npm registry are future implementations. It is dispatched **statically**:
/// one concrete client today, an enum per proxy once a second protocol exists, never a boxed object,
/// so proxying costs nothing over calling the client directly. Parsing the returned document is the
/// ecosystem driver's job; this trait only fetches.
///
/// Returns are written as `impl Future + Send` rather than `async fn` so callers can spawn the futures
/// on a multi-threaded runtime without the trait dictating auto-trait bounds.
pub trait UpstreamProtocol {
    /// Fetch a project's index document, conditional on `etag`.
    fn fetch_project(
        &self,
        project: &str,
        etag: Option<&str>,
    ) -> impl std::future::Future<Output = Result<SimpleResponse, UpstreamError>> + Send;

    /// Fetch the full project list.
    fn fetch_index(&self) -> impl std::future::Future<Output = Result<SimpleResponse, UpstreamError>> + Send;

    /// Fetch a file's bytes by URL.
    fn fetch_bytes(&self, url: &str) -> impl std::future::Future<Output = Result<Bytes, UpstreamError>> + Send;
}

impl UpstreamProtocol for UpstreamClient {
    fn fetch_project(
        &self,
        project: &str,
        etag: Option<&str>,
    ) -> impl std::future::Future<Output = Result<SimpleResponse, UpstreamError>> + Send {
        Self::fetch_project(self, project, etag)
    }

    fn fetch_index(&self) -> impl std::future::Future<Output = Result<SimpleResponse, UpstreamError>> + Send {
        Self::fetch_index(self)
    }

    fn fetch_bytes(&self, url: &str) -> impl std::future::Future<Output = Result<Bytes, UpstreamError>> + Send {
        Self::fetch_bytes(self, url)
    }
}
