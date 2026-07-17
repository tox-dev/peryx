//! The upstream HTTP client.

mod error;
mod netrc;
pub mod retry;

mod response;

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures_core::Stream;
use reqwest::header::{
    ACCEPT, ACCEPT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, HeaderMap, HeaderName, IF_NONE_MATCH, RANGE,
};
use url::Url;

use self::response::header_str;
use self::retry::{
    MAX_RETRIES, should_retry_error, should_retry_status, sleep_before_retry_status, sleep_before_retry_str,
};

pub use self::error::{RangeError, UpstreamError};
pub use self::netrc::{Netrc, NetrcError};
pub use self::response::FileHead;

const USER_AGENT: &str = concat!("peryx/", env!("CARGO_PKG_VERSION"));
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How peryx authenticates to a private upstream. `Basic` covers pypi.org tokens (`__token__` +
/// token) and Artifactory/GitLab username/password; `Bearer` covers access/identity tokens.
#[derive(Clone, Default, PartialEq, Eq)]
pub enum Auth {
    #[default]
    None,
    Basic {
        username: String,
        password: String,
    },
    Bearer(String),
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::None => "None",
            Self::Basic { .. } => "Basic(..)",
            Self::Bearer(_) => "Bearer(..)",
        })
    }
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

/// The result of the most recent connection attempt to an upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reachability {
    Unknown,
    Reachable,
    Unreachable,
}

impl Reachability {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Reachable => "reachable",
            Self::Unreachable => "unreachable",
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
    reachability: Arc<AtomicU8>,
}

const RANGE_UNKNOWN: u8 = 0;
const RANGE_SUPPORTED: u8 = 1;
const RANGE_UNSUPPORTED: u8 = 2;
const REACHABILITY_UNKNOWN: u8 = 0;
const REACHABILITY_REACHABLE: u8 = 1;
const REACHABILITY_UNREACHABLE: u8 = 2;

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
            reachability: Arc::new(AtomicU8::new(REACHABILITY_UNKNOWN)),
        })
    }

    fn authenticate(&self, request: reqwest::RequestBuilder, url: &Url) -> reqwest::RequestBuilder {
        match &self.auth {
            Auth::None => request,
            _ if !same_origin(&self.base, url) => request,
            Auth::Basic { username, password } => request.basic_auth(username, Some(password)),
            Auth::Bearer(token) => request.bearer_auth(token),
        }
    }

    /// Open a connection to the upstream host ahead of traffic, so the first real request skips
    /// the TCP and TLS handshakes. Failures are the first real request's problem to report.
    pub async fn warm(&self) {
        self.reachability.store(
            if self
                .authenticate(self.http.head(self.base.clone()), &self.base)
                .send()
                .await
                .is_ok()
            {
                REACHABILITY_REACHABLE
            } else {
                REACHABILITY_UNREACHABLE
            },
            Ordering::Relaxed,
        );
    }

    /// Whether the most recent request reached the upstream host.
    #[must_use]
    pub fn reachability(&self) -> Reachability {
        match self.reachability.load(Ordering::Relaxed) {
            REACHABILITY_REACHABLE => Reachability::Reachable,
            REACHABILITY_UNREACHABLE => Reachability::Unreachable,
            _ => Reachability::Unknown,
        }
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
        let url = Url::parse(url)?;
        let response = self
            .send_with_retry(|| self.authenticate(self.bulk.get(url.clone()).header(ACCEPT_ENCODING, "identity"), &url))
            .await?
            .error_for_status()?;
        Ok(response.bytes_stream().map_err(UpstreamError::from))
    }

    /// Fetch a file's bytes from an absolute URL.
    ///
    /// # Errors
    /// Returns [`UpstreamError::Http`] if the request fails or answers a non-success status.
    pub async fn fetch_bytes(&self, url: &str) -> Result<Bytes, UpstreamError> {
        let url = Url::parse(url)?;
        let mut attempt = 0;
        loop {
            let response = self
                .send_with_retry(|| {
                    self.authenticate(self.bulk.get(url.clone()).header(ACCEPT_ENCODING, "identity"), &url)
                })
                .await?
                .error_for_status()?;
            match response.bytes().await {
                Ok(bytes) => return Ok(bytes),
                Err(err) if should_retry_error(&err) && attempt < MAX_RETRIES => {
                    sleep_before_retry_str(url.as_str(), attempt, &err).await;
                    attempt += 1;
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    /// Fetch a file's bytes from an absolute URL without reading more than `limit` bytes.
    ///
    /// # Errors
    /// Returns [`UpstreamError::ResponseTooLarge`] if the response exceeds `limit`, or
    /// [`UpstreamError::Http`] if the request fails or answers a non-success status.
    pub async fn fetch_bytes_limited(&self, url: &str, limit: usize) -> Result<Bytes, UpstreamError> {
        use futures_util::TryStreamExt as _;

        let url = Url::parse(url)?;
        let mut attempt = 0;
        loop {
            let response = self
                .send_with_retry(|| {
                    self.authenticate(self.bulk.get(url.clone()).header(ACCEPT_ENCODING, "identity"), &url)
                })
                .await?
                .error_for_status()?;
            let content_length = response.content_length();
            if content_length.is_some_and(|length| length > u64::try_from(limit).unwrap_or(u64::MAX)) {
                return Err(UpstreamError::ResponseTooLarge { limit });
            }
            let mut bytes = BytesMut::with_capacity(
                content_length
                    .and_then(|length| usize::try_from(length).ok())
                    .unwrap_or_default(),
            );
            let mut stream = response.bytes_stream();
            loop {
                match stream.try_next().await {
                    Ok(Some(chunk)) if chunk.len() > limit - bytes.len() => {
                        return Err(UpstreamError::ResponseTooLarge { limit });
                    }
                    Ok(Some(chunk)) => bytes.extend_from_slice(&chunk),
                    Ok(None) => return Ok(bytes.freeze()),
                    Err(err) if should_retry_error(&err) && attempt < MAX_RETRIES => {
                        sleep_before_retry_str(url.as_str(), attempt, &err).await;
                        attempt += 1;
                        break;
                    }
                    Err(err) => return Err(err.into()),
                }
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
        let url = Url::parse(url).map_err(UpstreamError::from)?;
        let response = self
            .authenticate(self.http.head(url.clone()), &url)
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
        let url = Url::parse(url).map_err(UpstreamError::from)?;
        let response = self
            .authenticate(self.http.get(url.clone()), &url)
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

    /// The configured upstream base URL as a [`Url`], for an ecosystem layer that joins ecosystem
    /// paths onto it (the `PyPI` Simple client builds `{base}/{project}/`). Carries credential
    /// material if the configured URL did, so anything user-facing must redact first.
    #[must_use]
    pub const fn base(&self) -> &Url {
        &self.base
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

    /// Send a conditional `GET` to `url` with the caller's `Accept` and optional `If-None-Match`,
    /// run through the shared retry engine, and hand back the open response for the caller to read or
    /// stream. This is the neutral primitive an ecosystem's index-fetch layer (the `PyPI` Simple
    /// client) builds its document requests on; `304`/`404` are surfaced, not raised.
    ///
    /// # Errors
    /// Returns [`UpstreamError::Http`] if the request fails after exhausting retries.
    pub async fn send_conditional(
        &self,
        url: Url,
        accept: &str,
        etag: Option<&str>,
    ) -> Result<reqwest::Response, UpstreamError> {
        self.send_with_retry(|| {
            let mut request = self
                .authenticate(self.http.get(url.clone()), &url)
                .header(ACCEPT, accept);
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
                Ok(response) => {
                    self.reachability.store(REACHABILITY_REACHABLE, Ordering::Relaxed);
                    return Ok(response);
                }
                Err(err) if should_retry_error(&err) && attempt < MAX_RETRIES => {
                    sleep_before_retry_str(err.url().map_or("unknown URL", Url::as_str), attempt, &err).await;
                    attempt += 1;
                }
                Err(err) => {
                    self.reachability.store(REACHABILITY_UNREACHABLE, Ordering::Relaxed);
                    return Err(err.into());
                }
            }
        }
    }
}

fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host() == right.host()
        && left.port_or_known_default() == right.port_or_known_default()
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
