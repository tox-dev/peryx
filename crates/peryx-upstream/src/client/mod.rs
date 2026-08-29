mod credential;
mod error;
mod exec;
mod guard;
mod netrc;
mod range;
pub mod retry;
mod tls;

mod response;

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{Duration, SystemTime};

use bytes::{Bytes, BytesMut};
use futures_core::Stream;
use reqwest::header::{
    ACCEPT, ACCEPT_ENCODING, CONTENT_LENGTH, CONTENT_RANGE, HeaderMap, IF_MODIFIED_SINCE, IF_NONE_MATCH, RANGE,
};
use url::Url;

use self::guard::OutboundGuard;
use self::range::RangeSuppressions;
use self::response::header_str;
use self::retry::{
    MAX_RETRIES, RETRY_WAIT_BUDGET, retry_after_at, retry_delay, should_retry_error, should_retry_status,
    sleep_before_retry_status, sleep_before_retry_str,
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
pub(crate) const RANGE_SUPPRESSION_CAPACITY: usize = 1_024;
pub(crate) const RANGE_SUPPRESSION_TTL: Duration = Duration::from_mins(5);

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

/// Authentication type without credential values.
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

/// Outcome of the most recent upstream connection attempt.
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

/// An HTTP client that blocks unsafe destinations and cross-origin credential forwarding.
#[derive(Debug, Clone)]
pub struct UpstreamClient {
    http: reqwest::Client,
    /// HTTP/1.1 gives concurrent artifact downloads separate congestion windows; HTTP/2 would
    /// multiplex them over one connection.
    bulk: reqwest::Client,
    cross_origin_http: reqwest::Client,
    cross_origin_bulk: reqwest::Client,
    base: Url,
    credentials: CredentialProvider,
    guard: OutboundGuard,
    range_suppressions: Arc<RangeSuppressions>,
    reachability: Arc<AtomicU8>,
}

const REACHABILITY_UNKNOWN: u8 = 0;
const REACHABILITY_REACHABLE: u8 = 1;
const REACHABILITY_UNREACHABLE: u8 = 2;

impl UpstreamClient {
    /// Uses no request authentication.
    ///
    /// # Errors
    /// Returns [`UpstreamError::Url`] if `base` is not a valid URL, or [`UpstreamError::Http`] if
    /// the HTTP client cannot be built.
    pub fn new(base: &str) -> Result<Self, UpstreamError> {
        Self::with_auth(base, Auth::None)
    }

    /// Uses fixed upstream authentication.
    ///
    /// # Errors
    /// Returns [`UpstreamError::Url`] if `base` is not a valid URL, or [`UpstreamError::Http`] if
    /// the HTTP client cannot be built.
    pub fn with_auth(base: &str, auth: Auth) -> Result<Self, UpstreamError> {
        Self::with_auth_and_tls(base, auth, &UpstreamTls::default())
    }

    /// Applies fixed authentication and per-upstream TLS material.
    ///
    /// # Errors
    /// Returns [`UpstreamError::Url`] if `base` is not a valid URL, or [`UpstreamError::Http`] if
    /// the TLS material is invalid or the HTTP clients cannot be built.
    pub fn with_auth_and_tls(base: &str, auth: Auth, tls: &UpstreamTls) -> Result<Self, UpstreamError> {
        Self::with_auth_and_tls_for_origin(base, auth, tls, base)
    }

    /// Restricts the TLS identity to bases that share `identity_origin`. Custom trust roots apply
    /// across origins.
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

    /// Clones of `credentials` share one refresh gate and generation, allowing an artifact mirror
    /// to use its metadata source's provider.
    ///
    /// `trusted_hosts` permits private artifact servers; the operator-configured `base` host is
    /// trusted without listing.
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
        // Installation is process-wide, so another caller may have installed the provider first.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
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
        let (cross_origin_http, cross_origin_bulk) = if matches!((include_identity, tls.has_identity()), (true, true)) {
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
            range_suppressions: Arc::new(RangeSuppressions::default()),
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

    /// Builds a guarded request without attaching configured credentials. Protocol drivers with
    /// their own authentication exchange can add credentials at that boundary.
    ///
    /// # Errors
    /// Returns [`UpstreamError::Url`] for an invalid URL or [`UpstreamError::BlockedDestination`]
    /// for a disallowed destination.
    pub fn request_without_auth(
        &self,
        method: reqwest::Method,
        url: &str,
    ) -> Result<reqwest::RequestBuilder, UpstreamError> {
        let url = Url::parse(url)?;
        self.guard.check_url(&url)?;
        Ok(self.http(&url).request(method, url))
    }

    /// Opens a connection before traffic so the first request skips TCP and TLS handshakes.
    /// A failed warm-up does not fail future requests.
    pub async fn warm(&self) {
        let Ok(credentials) = self.credentials.credential().await else {
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

    #[must_use]
    pub fn reachability(&self) -> Reachability {
        match self.reachability.load(Ordering::Relaxed) {
            REACHABILITY_UNKNOWN => Reachability::Unknown,
            REACHABILITY_REACHABLE => Reachability::Reachable,
            _ => Reachability::Unreachable,
        }
    }

    /// Streams bytes from an absolute URL.
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
            .send_with_retry(&mut |auth| {
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

    /// Fetches bytes from an absolute URL.
    ///
    /// # Errors
    /// Returns [`UpstreamError::Credential`] when refresh fails, or [`UpstreamError::Http`] when the
    /// request fails or answers a non-success status.
    pub async fn fetch_bytes(&self, url: &str) -> Result<Bytes, UpstreamError> {
        let url = Url::parse(url)?;
        self.guard.check_url(&url)?;
        let mut retries = 0..MAX_RETRIES;
        loop {
            let response = self
                .send_with_retry(&mut |auth| {
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
                Err(err) => match (should_retry_error(&err), retries.next()) {
                    (true, Some(attempt)) => sleep_before_retry_str(url.as_str(), attempt, &err).await,
                    _ => return Err(err.into()),
                },
            }
        }
    }

    /// Reads at most `limit` bytes from an absolute URL.
    ///
    /// # Errors
    /// Returns [`UpstreamError::ResponseTooLarge`] if the response exceeds `limit`, or
    /// [`UpstreamError::Credential`] when refresh fails, or [`UpstreamError::Http`] when the request
    /// fails or answers a non-success status.
    pub async fn fetch_bytes_limited(&self, url: &str, limit: usize) -> Result<Bytes, UpstreamError> {
        use futures_util::TryStreamExt as _;

        let url = Url::parse(url)?;
        self.guard.check_url(&url)?;
        let mut retries = 0..MAX_RETRIES;
        loop {
            let response = self
                .send_with_retry(&mut |auth| {
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
                    Err(err) => match (should_retry_error(&err), retries.next()) {
                        (true, Some(attempt)) => {
                            sleep_before_retry_str(url.as_str(), attempt, &err).await;
                            break;
                        }
                        _ => return Err(err.into()),
                    },
                }
            }
        }
    }

    /// Reads the representation length without relying on advisory `Accept-Ranges` metadata.
    ///
    /// # Errors
    /// Returns [`RangeError::Unsupported`] when the resource recently ignored a range or cannot
    /// provide length metadata, and [`RangeError::Upstream`] on other request failures.
    pub async fn head_file_for_range(&self, url: &str) -> Result<FileHead, RangeError> {
        let url = Url::parse(url).map_err(UpstreamError::from)?;
        self.guard.check_url(&url).map_err(RangeError::from)?;
        if self.range_suppressions.contains(&url) {
            return Err(RangeError::Unsupported);
        }
        let response = self
            .send_with_retry(&mut |auth| {
                self.authenticate(self.http(&url).head(url.clone()), &url, auth)
                    .header(ACCEPT_ENCODING, "identity")
            })
            .await
            .map_err(RangeError::from)?;
        if response.status().is_success() {
            return match header_str(response.headers(), &CONTENT_LENGTH).and_then(|value| value.parse().ok()) {
                Some(len) => Ok(FileHead { len }),
                None => self.probe_file_for_range(&url).await,
            };
        }
        if matches!(
            response.status(),
            reqwest::StatusCode::METHOD_NOT_ALLOWED | reqwest::StatusCode::NOT_IMPLEMENTED
        ) {
            return self.probe_file_for_range(&url).await;
        }
        if matches!(
            response.status(),
            reqwest::StatusCode::BAD_REQUEST | reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::NOT_FOUND
        ) {
            return Err(RangeError::Unsupported);
        }
        response.error_for_status().map_err(UpstreamError::from)?;
        Err(RangeError::Invalid("HEAD returned a non-success response".to_owned()))
    }

    /// Fetches the inclusive byte range `start..=end` within `memory_limit`.
    ///
    /// # Errors
    /// Returns [`RangeError::Unsupported`] or [`RangeError::Invalid`] when upstream cannot satisfy
    /// the requested range or it exceeds `memory_limit`, and [`RangeError::Upstream`] on other
    /// request failures.
    pub async fn fetch_range(&self, url: &str, start: u64, end: u64, memory_limit: usize) -> Result<Bytes, RangeError> {
        let (range_len, expected_len) = range_lengths(start, end, memory_limit)?;
        let url = Url::parse(url).map_err(UpstreamError::from)?;
        self.guard.check_url(&url).map_err(RangeError::from)?;
        let response = self.request_range(&url, start, end).await?;
        validate_content_range(response.headers(), start, end)?;
        read_range_body(response, range_len, expected_len).await
    }

    async fn probe_file_for_range(&self, url: &Url) -> Result<FileHead, RangeError> {
        let response = self.request_range(url, 0, 0).await?;
        let len = validate_content_range(response.headers(), 0, 0)?
            .ok_or_else(|| RangeError::Invalid("range probe returned an unknown representation length".to_owned()))?;
        read_range_body(response, 1, 1).await?;
        Ok(FileHead { len })
    }

    async fn request_range(&self, url: &Url, start: u64, end: u64) -> Result<reqwest::Response, RangeError> {
        if self.range_suppressions.contains(url) {
            return Err(RangeError::Unsupported);
        }
        let response = self
            .send_with_retry(&mut |auth| {
                self.authenticate(self.http(url).get(url.clone()), url, auth)
                    .header(ACCEPT_ENCODING, "identity")
                    .header(RANGE, format!("bytes={start}-{end}"))
            })
            .await
            .map_err(RangeError::from)?;
        match response.status() {
            reqwest::StatusCode::PARTIAL_CONTENT => Ok(response),
            reqwest::StatusCode::OK => {
                self.range_suppressions.insert(url.clone());
                Err(RangeError::Unsupported)
            }
            reqwest::StatusCode::RANGE_NOT_SATISFIABLE => Err(RangeError::Unsupported),
            _ => {
                response.error_for_status().map_err(UpstreamError::from)?;
                Err(RangeError::Invalid(
                    "range request returned a non-206 success".to_owned(),
                ))
            }
        }
    }

    /// Removes user info, query, and fragment for display.
    #[must_use]
    pub fn redacted_base_url(&self) -> String {
        redact_url(self.base.as_ref())
    }

    /// Includes a trailing slash and may contain credentials; redact before display.
    #[must_use]
    pub fn base_url(&self) -> &str {
        self.base.as_str()
    }

    /// Callers join source paths onto this base [`Url`]. It may contain credentials; redact before
    /// display.
    #[must_use]
    pub const fn base(&self) -> &Url {
        &self.base
    }

    #[must_use]
    pub const fn auth(&self) -> &CredentialProvider {
        &self.credentials
    }

    /// Omits credential values.
    #[must_use]
    pub fn auth_status(&self) -> AuthStatus {
        match self.credentials.snapshot().configured_auth() {
            Auth::None => AuthStatus::None,
            Auth::Basic { .. } => AuthStatus::Basic,
            Auth::Bearer(_) => AuthStatus::Bearer,
        }
    }

    /// Returns the last credential without applying its refresh deadline.
    ///
    /// # Errors
    /// Returns the provider's last redacted refresh error under `fail` policy.
    pub fn current_credential(&self) -> Result<Arc<CredentialSnapshot>, UpstreamError> {
        self.credentials.current().map_err(UpstreamError::from)
    }

    /// Sends a retryable `GET` with `Accept` and optional `If-None-Match`. The caller receives the
    /// open response, including `304` and `404` statuses.
    ///
    /// # Errors
    /// Returns [`UpstreamError::BlockedDestination`] when the initial URL violates the outbound
    /// policy, [`UpstreamError::Credential`] when refresh fails, or [`UpstreamError::Http`] when the
    /// request fails after exhausting retries.
    pub async fn send_conditional(
        &self,
        url: Url,
        accept: &str,
        etag: Option<&str>,
    ) -> Result<reqwest::Response, UpstreamError> {
        self.send_validated(url, accept, etag, None).await
    }

    /// Sends a conditional metadata request. `If-None-Match` takes precedence over modification
    /// time.
    ///
    /// # Errors
    /// Returns [`UpstreamError::BlockedDestination`] when the initial URL violates the outbound
    /// policy, [`UpstreamError::Credential`] when refresh fails, or [`UpstreamError::Http`] when the
    /// request fails after exhausting retries.
    pub async fn send_validated(
        &self,
        url: Url,
        accept: &str,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> Result<reqwest::Response, UpstreamError> {
        self.guard.check_url(&url)?;
        self.send_with_retry(&mut |auth| {
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
        request: &mut (dyn FnMut(&Auth) -> reqwest::RequestBuilder + Send),
    ) -> Result<reqwest::Response, UpstreamError> {
        let mut retries = 0..MAX_RETRIES;
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
                Ok(response) => {
                    if should_retry_status(response.status()) {
                        let server_delay = retry_after_at(response.headers(), SystemTime::now());
                        if server_delay.is_some_and(|delay| delay > RETRY_WAIT_BUDGET) {
                            self.reachability.store(REACHABILITY_REACHABLE, Ordering::Relaxed);
                            return Ok(response);
                        }
                        let Some(attempt) = retries.next() else {
                            self.reachability.store(REACHABILITY_REACHABLE, Ordering::Relaxed);
                            return Ok(response);
                        };
                        let url = response.url().clone();
                        let status = response.status();
                        sleep_before_retry_status(&url, status, server_delay.unwrap_or_else(|| retry_delay(attempt)))
                            .await;
                        continue;
                    }
                    self.reachability.store(REACHABILITY_REACHABLE, Ordering::Relaxed);
                    return Ok(response);
                }
                Err(err) => {
                    if let (true, Some(attempt)) = (should_retry_error(&err), retries.next()) {
                        sleep_before_retry_str(err.url().map_or("unknown URL", Url::as_str), attempt, &err).await;
                        continue;
                    }
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

/// Rejects redirect hops to disallowed IP literals and prevents a TLS identity from crossing
/// origins. The resolver checks hostname destinations at connection time.
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
        if attempt.previous().get(MAX_REDIRECTS).is_some() {
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

/// Removes user info, query, and fragment before display.
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

pub(crate) fn range_lengths(start: u64, end: u64, memory_limit: usize) -> Result<(u64, usize), RangeError> {
    if end < start {
        return Err(RangeError::Invalid(format!("start {start} is after end {end}")));
    }
    let Some(range_len) = (end - start).checked_add(1) else {
        return Err(RangeError::Invalid("requested range length overflowed".to_owned()));
    };
    if range_len > u64::try_from(memory_limit).expect("memory limits fit in range lengths") {
        return Err(RangeError::Invalid(format!(
            "requested range of {range_len} bytes exceeds the {memory_limit}-byte memory limit"
        )));
    }
    let expected_len = usize::try_from(range_len).expect("range length is within the memory limit");
    Ok((range_len, expected_len))
}

fn validate_content_range(headers: &HeaderMap, start: u64, end: u64) -> Result<Option<u64>, RangeError> {
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
    // RFC 9110 permits "*" or a decimal greater than last-byte-pos for complete-length.
    let total = if total == "*" {
        None
    } else {
        Some(total.parse::<u64>().map_err(|_| {
            RangeError::Invalid(format!(
                "invalid Content-Range total for bytes {start}-{end}, got {value:?}"
            ))
        })?)
    };
    if total.is_some_and(|total| total <= end) {
        return Err(RangeError::Invalid(format!(
            "invalid Content-Range total for bytes {start}-{end}, got {value:?}"
        )));
    }
    Ok(total)
}

async fn read_range_body(
    mut response: reqwest::Response,
    range_len: u64,
    expected_len: usize,
) -> Result<Bytes, RangeError> {
    if let Some(content_length) = response.content_length()
        && content_length != range_len
    {
        return Err(RangeError::Invalid(format!(
            "expected {expected_len} bytes, received Content-Length {content_length}"
        )));
    }
    let mut bytes = BytesMut::with_capacity(expected_len);
    while let Some(chunk) = response.chunk().await.map_err(UpstreamError::from)? {
        if chunk.len() > expected_len - bytes.len() {
            return Err(RangeError::Invalid(format!(
                "response body exceeds the expected {expected_len} bytes"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.len() != expected_len {
        return Err(RangeError::Invalid(format!(
            "expected {expected_len} bytes, received {}",
            bytes.len()
        )));
    }
    Ok(bytes.freeze())
}
