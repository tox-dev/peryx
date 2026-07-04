//! The upstream HTTP client.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_core::Stream;
use reqwest::StatusCode;
use reqwest::header::{
    ACCEPT, ACCEPT_ENCODING, CACHE_CONTROL, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG, HeaderMap, HeaderName,
    IF_NONE_MATCH, RANGE,
};
use url::Url;

/// The `Accept` header velodex sends upstream: PEP 691 JSON first, then PEP 503 HTML.
const ACCEPT_SIMPLE: &str =
    "application/vnd.pypi.simple.v1+json, application/vnd.pypi.simple.v1+html;q=0.2, text/html;q=0.01";
const USER_AGENT: &str = concat!("velodex/", env!("CARGO_PKG_VERSION"));
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RETRIES: u32 = 2;
const RETRY_BASE_MILLIS: u64 = 100;
const RETRY_CAP_MILLIS: u64 = 2_000;

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

/// The parts of an artifact `HEAD` response needed before range reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileHead {
    pub len: u64,
}

/// An error from the range-read path.
#[derive(Debug, thiserror::Error)]
pub enum RangeError {
    #[error(transparent)]
    Upstream(#[from] UpstreamError),
    #[error("upstream does not support byte range requests")]
    Unsupported,
    #[error("upstream returned an invalid byte range response: {0}")]
    Invalid(String),
}

impl RangeError {
    /// Whether Velodex should stop trying ranges for this index and fall back to full downloads.
    #[must_use]
    pub const fn disables_ranges(&self) -> bool {
        matches!(self, Self::Unsupported | Self::Invalid(_))
    }
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
    #[error("missing upstream Simple API Content-Type from {url}")]
    MissingContentType { url: Url },
    #[error("unsupported upstream Simple API Content-Type {content_type:?} from {url}")]
    UnsupportedContentType { url: Url, content_type: String },
}

impl UpstreamError {
    /// The HTTP status attached to a transport error, when reqwest has one.
    #[must_use]
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Http(err) => err.status().map(|status| status.as_u16()),
            Self::Url(_) | Self::MissingContentType { .. } | Self::UnsupportedContentType { .. } => None,
        }
    }
}

impl UpstreamError {
    /// Error text safe for user-visible responses: status and failure class, without URLs that may
    /// contain credentials or signed query strings.
    #[must_use]
    pub fn user_message(&self) -> String {
        match self {
            Self::Url(err) => format!("invalid upstream URL: {err}"),
            Self::Http(err) if let Some(status) = err.status() => format!("upstream returned {status}"),
            Self::Http(err) if err.is_timeout() => "upstream request timed out".to_owned(),
            Self::Http(err) if err.is_connect() => "upstream connection failed".to_owned(),
            Self::Http(err) if err.is_decode() => "upstream response could not be decoded".to_owned(),
            Self::Http(_) => "upstream request failed".to_owned(),
            Self::MissingContentType { .. } => "upstream response missed Simple API Content-Type".to_owned(),
            Self::UnsupportedContentType { .. } => "upstream returned unsupported Simple API Content-Type".to_owned(),
        }
    }
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

    /// The authentication scheme without credential material.
    #[must_use]
    pub const fn auth_status(&self) -> AuthStatus {
        match &self.auth {
            Auth::None => AuthStatus::None,
            Auth::Basic { .. } => AuthStatus::Basic,
            Auth::Bearer(_) => AuthStatus::Bearer,
        }
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

fn header_str(headers: &HeaderMap, name: &HeaderName) -> Option<String> {
    headers.get(name)?.to_str().ok().map(str::to_owned)
}

fn simple_head(response: reqwest::Response) -> Result<SimpleHead, UpstreamError> {
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

fn should_retry_status(status: StatusCode) -> bool {
    status.is_server_error() || matches!(status, StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS)
}

fn should_retry_error(err: &reqwest::Error) -> bool {
    err.is_timeout() || err.is_connect() || err.is_body() || err.is_decode()
}

async fn sleep_before_retry(url: &Url, attempt: u32, err: &reqwest::Error) {
    sleep_before_retry_str(url.as_str(), attempt, err).await;
}

async fn sleep_before_retry_str(url: &str, attempt: u32, err: &reqwest::Error) {
    let delay = retry_delay(attempt);
    tracing::debug!(url, error = ?err, delay_ms = delay.as_millis(), "upstream request failed, retrying");
    tokio::time::sleep(delay).await;
}

async fn sleep_before_retry_status(url: &Url, attempt: u32, status: StatusCode) {
    let delay = retry_delay(attempt);
    tracing::debug!(%url, %status, delay_ms = delay.as_millis(), "upstream returned retryable status");
    tokio::time::sleep(delay).await;
}

fn retry_delay(attempt: u32) -> Duration {
    let cap = RETRY_CAP_MILLIS.min(RETRY_BASE_MILLIS.saturating_mul(1_u64 << attempt.min(20)));
    let floor = cap / 2;
    Duration::from_millis(floor + jitter(cap - floor + 1))
}

fn jitter(modulus: u64) -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| u64::from(duration.subsec_nanos()) % modulus)
}

/// The upstream fetch protocol a proxy index speaks.
///
/// A proxy revalidates and caches an upstream's index documents and files. This trait is the seam
/// that logic plugs into: [`UpstreamClient`] implements the `PyPI` PEP 503/691 simple API, and an OCI
/// registry (`/v2/`) or an npm registry are future implementations. It is dispatched **statically** —
/// one concrete client today, an enum per proxy once a second protocol exists, never a boxed object —
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
