//! The upstream HTTP client.

mod credential;
mod error;
mod exec;
pub(crate) mod guard;
mod netrc;
pub mod retry;
mod tls;

mod response;

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use futures_core::Stream;
use reqwest::header::{
    ACCEPT, ACCEPT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, HeaderMap, HeaderName, IF_MODIFIED_SINCE, IF_NONE_MATCH,
    RANGE,
};
use url::Url;

use self::guard::OutboundGuard;
use self::response::header_str;
use self::retry::{
    MAX_RETRIES, should_retry_error, should_retry_status, sleep_before_retry_status, sleep_before_retry_str,
};

pub use self::credential::{
    CredentialError, CredentialFailure, CredentialIdentity, CredentialProvider, CredentialProviderId,
    CredentialRefresh, CredentialSnapshot,
};
pub use self::error::{RangeError, UpstreamError};
pub use self::exec::{CredentialScope, ExecCredentialConfig, ExecCredentialConfigError, ExecCredentialProviderError};
pub use self::netrc::{Netrc, NetrcError};
pub use self::response::FileHead;
pub use self::tls::{UpstreamTls, UpstreamTlsError};

const USER_AGENT: &str = concat!("peryx/", env!("CARGO_PKG_VERSION"));
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How peryx authenticates to a private upstream. `Basic` carries a username and secret;
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
    cross_origin_http: reqwest::Client,
    cross_origin_bulk: reqwest::Client,
    base: Url,
    credentials: CredentialProvider,
    guard: OutboundGuard,
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
    /// Build an unauthenticated client for `base`.
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
        Self::with_auth_and_tls(base, auth, &UpstreamTls::default())
    }

    /// Build a client for `base` with HTTP authentication and per-upstream TLS material.
    ///
    /// # Errors
    /// Returns [`UpstreamError::Url`] if `base` is not a valid URL, or [`UpstreamError::Http`] if
    /// the TLS material is invalid or the HTTP clients cannot be built.
    pub fn with_auth_and_tls(base: &str, auth: Auth, tls: &UpstreamTls) -> Result<Self, UpstreamError> {
        Self::with_auth_and_tls_for_origin(base, auth, tls, base)
    }

    /// Build a client whose TLS identity is available only when `base` shares `identity_origin`.
    /// Custom trust roots remain available on another origin.
    ///
    /// # Errors
    /// Returns [`UpstreamError::Url`] if either origin is invalid, or [`UpstreamError::Http`] if
    /// the TLS material is invalid or the HTTP clients cannot be built.
    pub fn with_auth_and_tls_for_origin(
        base: &str,
        auth: Auth,
        tls: &UpstreamTls,
        identity_origin: &str,
    ) -> Result<Self, UpstreamError> {
        Self::with_credentials_and_tls_for_origin(base, CredentialProvider::fixed(auth), tls, identity_origin, &[])
    }

    /// Build a client with refreshable credentials. Clones of `credentials` share one refresh gate
    /// and generation, which lets an artifact mirror use its metadata source's credential provider.
    ///
    /// `trusted_hosts` names private artifact servers the outbound policy may reach even though they
    /// do not resolve to a public address; the configured `base` host is trusted without listing.
    ///
    /// # Errors
    /// Returns [`UpstreamError::Url`] if either origin is invalid, or [`UpstreamError::Http`] if
    /// the TLS material is invalid or the HTTP clients cannot be built.
    pub fn with_credentials_and_tls_for_origin(
        base: &str,
        credentials: CredentialProvider,
        tls: &UpstreamTls,
        identity_origin: &str,
        trusted_hosts: &[String],
    ) -> Result<Self, UpstreamError> {
        // Pin the ring crypto provider: unlike aws-lc it is pure Rust plus portable assembly, so
        // every release target cross-compiles without a C toolchain. Err means already installed.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut base = Url::parse(base)?;
        let include_identity = same_origin(&base, &Url::parse(identity_origin)?);
        if !base.path().ends_with('/') {
            let with_slash = format!("{}/", base.path());
            base.set_path(&with_slash);
        }
        let guard = OutboundGuard::new(&base, trusted_hosts);
        let http = configure_http_client(
            tls.apply(reqwest::Client::builder(), include_identity),
            guarded_redirect_policy(&base, tls, include_identity, &guard),
            &guard,
        )
        .http2_adaptive_window(true)
        .build()?;
        let bulk = configure_http_client(
            tls.apply(reqwest::Client::builder(), include_identity),
            guarded_redirect_policy(&base, tls, include_identity, &guard),
            &guard,
        )
        .http1_only()
        .build()?;
        let (cross_origin_http, cross_origin_bulk) = if include_identity && tls.has_identity() {
            (
                configure_http_client(
                    tls.apply(reqwest::Client::builder(), false),
                    guarded_redirect_policy(&base, tls, false, &guard),
                    &guard,
                )
                .http2_adaptive_window(true)
                .build()?,
                configure_http_client(
                    tls.apply(reqwest::Client::builder(), false),
                    guarded_redirect_policy(&base, tls, false, &guard),
                    &guard,
                )
                .http1_only()
                .build()?,
            )
        } else {
            (http.clone(), bulk.clone())
        };
        Ok(Self {
            http,
            bulk,
            cross_origin_http,
            cross_origin_bulk,
            base,
            credentials,
            guard,
            range_support: Arc::new(AtomicU8::new(RANGE_UNKNOWN)),
            reachability: Arc::new(AtomicU8::new(REACHABILITY_UNKNOWN)),
        })
    }

    fn authenticate(&self, request: reqwest::RequestBuilder, url: &Url, auth: &Auth) -> reqwest::RequestBuilder {
        match auth {
            Auth::None => request,
            _ if !same_origin(&self.base, url) => request,
            Auth::Basic { username, password } => request.basic_auth(username, Some(password)),
            Auth::Bearer(token) => request.bearer_auth(token),
        }
    }

    fn http(&self, url: &Url) -> &reqwest::Client {
        if same_origin(&self.base, url) {
            &self.http
        } else {
            &self.cross_origin_http
        }
    }

    fn bulk(&self, url: &Url) -> &reqwest::Client {
        if same_origin(&self.base, url) {
            &self.bulk
        } else {
            &self.cross_origin_bulk
        }
    }

    /// Open a connection to the upstream host ahead of traffic, so the first real request skips
    /// the TCP and TLS handshakes. Failures are the first real request's problem to report.
    pub async fn warm(&self) {
        let Ok(credentials) = self.credentials.credential().await else {
            self.reachability.store(REACHABILITY_UNREACHABLE, Ordering::Relaxed);
            return;
        };
        self.reachability.store(
            if self
                .authenticate(self.http.head(self.base.clone()), &self.base, credentials.auth())
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
    /// Returns [`UpstreamError::Credential`] when refresh fails, or [`UpstreamError::Http`] when the
    /// request fails or answers a non-success status.
    pub async fn stream_bytes(
        &self,
        url: &str,
    ) -> Result<impl Stream<Item = Result<Bytes, UpstreamError>> + Send + use<>, UpstreamError> {
        use futures_util::TryStreamExt as _;
        let url = Url::parse(url)?;
        self.guard.check_url(&url)?;
        let response = self
            .send_with_retry(|auth| {
                self.authenticate(
                    self.bulk(&url).get(url.clone()).header(ACCEPT_ENCODING, "identity"),
                    &url,
                    auth,
                )
            })
            .await?
            .error_for_status()?;
        Ok(response.bytes_stream().map_err(UpstreamError::from))
    }

    /// Fetch a file's bytes from an absolute URL.
    ///
    /// # Errors
    /// Returns [`UpstreamError::Credential`] when refresh fails, or [`UpstreamError::Http`] when the
    /// request fails or answers a non-success status.
    pub async fn fetch_bytes(&self, url: &str) -> Result<Bytes, UpstreamError> {
        let url = Url::parse(url)?;
        self.guard.check_url(&url)?;
        let mut attempt = 0;
        loop {
            let response = self
                .send_with_retry(|auth| {
                    self.authenticate(
                        self.bulk(&url).get(url.clone()).header(ACCEPT_ENCODING, "identity"),
                        &url,
                        auth,
                    )
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
    /// [`UpstreamError::Credential`] when refresh fails, or [`UpstreamError::Http`] when the request
    /// fails or answers a non-success status.
    pub async fn fetch_bytes_limited(&self, url: &str, limit: usize) -> Result<Bytes, UpstreamError> {
        use futures_util::TryStreamExt as _;

        let url = Url::parse(url)?;
        self.guard.check_url(&url)?;
        let mut attempt = 0;
        loop {
            let response = self
                .send_with_retry(|auth| {
                    self.authenticate(
                        self.bulk(&url).get(url.clone()).header(ACCEPT_ENCODING, "identity"),
                        &url,
                        auth,
                    )
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
        self.guard.check_url(&url).map_err(RangeError::from)?;
        let response = self
            .send_with_retry(|auth| {
                self.authenticate(self.http(&url).head(url.clone()), &url, auth)
                    .header(ACCEPT_ENCODING, "identity")
            })
            .await
            .map_err(RangeError::from)?;
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
        self.guard.check_url(&url).map_err(RangeError::from)?;
        let response = self
            .send_with_retry(|auth| {
                self.authenticate(self.http(&url).get(url.clone()), &url, auth)
                    .header(ACCEPT_ENCODING, "identity")
                    .header(RANGE, format!("bytes={start}-{end}"))
            })
            .await
            .map_err(RangeError::from)?;
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
    /// paths onto it. Carries credential
    /// material if the configured URL did, so anything user-facing must redact first.
    #[must_use]
    pub const fn base(&self) -> &Url {
        &self.base
    }

    /// The credential provider shared by ecosystem-specific authentication flows.
    #[must_use]
    pub const fn auth(&self) -> &CredentialProvider {
        &self.credentials
    }

    /// The authentication scheme without credential material.
    #[must_use]
    pub fn auth_status(&self) -> AuthStatus {
        match self.credentials.snapshot().configured_auth() {
            Auth::None => AuthStatus::None,
            Auth::Basic { .. } => AuthStatus::Basic,
            Auth::Bearer(_) => AuthStatus::Bearer,
        }
    }

    /// Return the last resolved credential without checking its refresh deadline.
    ///
    /// # Errors
    /// Returns the provider's last redacted refresh error under `fail` policy.
    pub fn current_credential(&self) -> Result<Arc<CredentialSnapshot>, UpstreamError> {
        self.credentials.current().map_err(UpstreamError::from)
    }

    /// Send a conditional `GET` to `url` with the caller's `Accept` and optional `If-None-Match`,
    /// run through the shared retry engine, and hand back the open response for the caller to read or
    /// stream. This is the neutral primitive an ecosystem's index-fetch layer
    /// client) builds its document requests on; `304`/`404` are surfaced, not raised.
    ///
    /// # Errors
    /// Returns [`UpstreamError::Credential`] when refresh fails, or [`UpstreamError::Http`] when the
    /// request fails after exhausting retries.
    pub async fn send_conditional(
        &self,
        url: Url,
        accept: &str,
        etag: Option<&str>,
    ) -> Result<reqwest::Response, UpstreamError> {
        self.send_validated(url, accept, etag, None).await
    }

    /// Send a conditional metadata request. `If-None-Match` takes precedence; modification time is
    /// the fallback for upstreams that do not provide entity tags.
    ///
    /// # Errors
    /// Returns [`UpstreamError::Credential`] when refresh fails, or [`UpstreamError::Http`] when the
    /// request fails after exhausting retries.
    pub async fn send_validated(
        &self,
        url: Url,
        accept: &str,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> Result<reqwest::Response, UpstreamError> {
        self.send_with_retry(|auth| {
            let mut request = self
                .authenticate(self.http(&url).get(url.clone()), &url, auth)
                .header(ACCEPT, accept);
            if let Some(etag) = etag {
                request = request.header(IF_NONE_MATCH, etag);
            } else if let Some(last_modified) = last_modified {
                request = request.header(IF_MODIFIED_SINCE, last_modified);
            }
            request
        })
        .await
    }

    async fn send_with_retry(
        &self,
        mut request: impl FnMut(&Auth) -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, UpstreamError> {
        let mut attempt = 0;
        let mut credential = self.credentials.credential().await.map_err(UpstreamError::from)?;
        let mut refreshed = false;
        loop {
            match request(credential.auth()).send().await {
                Ok(response) if response.status() == reqwest::StatusCode::UNAUTHORIZED && !refreshed => {
                    let generation = credential.generation();
                    let replacement = self
                        .credentials
                        .refresh_after_unauthorized(generation)
                        .await
                        .map_err(UpstreamError::from)?;
                    if replacement.generation() == generation {
                        self.reachability.store(REACHABILITY_REACHABLE, Ordering::Relaxed);
                        return Ok(response);
                    }
                    credential = replacement;
                    refreshed = true;
                }
                Ok(response) if should_retry_status(response.status()) && attempt < MAX_RETRIES => {
                    let url = response.url().clone();
                    let status = response.status();
                    sleep_before_retry_status(&url, attempt, status, response.headers()).await;
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

fn configure_http_client(
    builder: reqwest::ClientBuilder,
    redirect: reqwest::redirect::Policy,
    guard: &OutboundGuard,
) -> reqwest::ClientBuilder {
    builder
        .user_agent(USER_AGENT)
        .pool_max_idle_per_host(32)
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_keepalive(Duration::from_mins(1))
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .tls_version_min(reqwest::tls::Version::TLS_1_3)
        .dns_resolver(guard.clone())
        .redirect(redirect)
}

/// A redirect policy that enforces the outbound destination on every hop. It keeps the client's TLS
/// identity from following a cross-origin redirect and rejects a hop to a disallowed IP-literal host
/// before the connection; hostname hops are caught by the guarding resolver.
fn guarded_redirect_policy(
    base: &Url,
    tls: &UpstreamTls,
    include_identity: bool,
    guard: &OutboundGuard,
) -> reqwest::redirect::Policy {
    let restrict_identity = include_identity && tls.has_identity();
    let base = base.clone();
    let guard = guard.clone();
    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() > MAX_REDIRECTS {
            return attempt.error(TooManyRedirects);
        }
        if restrict_identity && !same_origin(&base, attempt.url()) {
            return attempt.error("upstream client identity cannot follow a cross-origin redirect");
        }
        match guard.check_url(attempt.url()) {
            Ok(()) => attempt.follow(),
            Err(err) => attempt.error(err),
        }
    })
}

const MAX_REDIRECTS: usize = 10;

#[derive(Debug)]
struct TooManyRedirects;

impl std::fmt::Display for TooManyRedirects {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("too many redirects")
    }
}

impl std::error::Error for TooManyRedirects {}

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
    let Some((actual, total)) = rest.split_once('/') else {
        return Err(RangeError::Invalid(format!("unexpected Content-Range {value:?}")));
    };
    let Some((actual_start, actual_end)) = actual.split_once('-') else {
        return Err(RangeError::Invalid(format!("unexpected Content-Range {value:?}")));
    };
    if actual_start.parse::<u64>().ok() != Some(start) || actual_end.parse::<u64>().ok() != Some(end) {
        return Err(RangeError::Invalid(format!(
            "expected Content-Range bytes {start}-{end}, got {value:?}"
        )));
    }
    // RFC 9110: complete-length is "*" (unknown) or a decimal strictly greater than last-byte-pos.
    if total != "*" && total.parse::<u64>().ok().is_none_or(|total| total <= end) {
        return Err(RangeError::Invalid(format!(
            "invalid Content-Range total for bytes {start}-{end}, got {value:?}"
        )));
    }
    Ok(())
}
