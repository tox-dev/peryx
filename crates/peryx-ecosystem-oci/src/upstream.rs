//! Pulls speak the distribution-spec pull API with the token-auth flow real registries require: an
//! anonymous request draws a `401` carrying `WWW-Authenticate: Bearer realm=…,service=…,scope=…`, the
//! client trades that challenge for a bearer token at the realm, then replays the request. Tokens are
//! cached per scope so a burst of blob pulls authenticates once, and a cached token that has expired
//! (a late `401`) re-runs the flow transparently.

use std::borrow::Cow;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderValue, Method, StatusCode};
use peryx_identity::strip_auth_scheme;
use peryx_upstream::{
    Auth, CredentialError, CredentialIdentity, CredentialProvider, CredentialProviderId, CredentialSnapshot,
};
use reqwest::Response;
use tokio::sync::{Mutex, watch};
use tokio::time::{Instant, timeout_at};

const TOKEN_FLIGHT_TIMEOUT: Duration = Duration::from_secs(30);

/// The manifest media types a puller accepts, mirroring containerd/docker: the Docker v2 schema and
/// manifest list, the OCI image manifest and index, then `*/*` so a registry that only knows one of
/// them still answers.
const ACCEPT_MANIFESTS: &str = "application/vnd.docker.distribution.manifest.v2+json, \
application/vnd.docker.distribution.manifest.list.v2+json, \
application/vnd.oci.image.manifest.v1+json, \
application/vnd.oci.image.index.v1+json, \
*/*";

/// A shared upstream fetcher: one HTTP client and one token cache for every configured OCI proxy.
///
/// `inflight` single-flights token exchanges: concurrent cold pulls that miss the same `(base, scope,
/// provider)` key elect one leader to trade the challenge for a token while the rest await its result,
/// so a burst that would otherwise fire one token request per pull fires one for the whole burst.
#[derive(Debug)]
pub struct Upstream {
    http: reqwest::Client,
    tokens: Mutex<HashMap<TokenCacheKey, CachedToken>>,
    inflight: Mutex<HashMap<TokenCacheKey, watch::Receiver<FlightState>>>,
    token_flight_timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TokenCacheKey {
    base: String,
    scope: String,
    provider: CredentialProviderId,
}

/// The result an in-flight token exchange publishes to the pulls awaiting it. `Failed` and a dropped
/// sender both send waiters back to elect a fresh leader, so a failed exchange never poisons the key.
#[derive(Debug, Clone)]
enum FlightState {
    Pending,
    Ready(String),
    Failed,
}

#[derive(Debug)]
struct CachedToken {
    credentials: CredentialIdentity,
    value: String,
}

/// Why an upstream pull did not yield bytes to serve.
#[derive(Debug)]
pub enum UpstreamError {
    /// The registry answered, but with a non-success status (forwarded to the client's error).
    Status(StatusCode),
    /// The registry throttled the pull (`429`), carrying its `Retry-After` when it sent one. Kept
    /// distinct from [`Self::Status`] so the client sees a `429` and the backoff hint, not a `502`.
    RateLimited(Option<String>),
    /// The transfer failed before a usable response (connection, TLS, timeout, decode).
    Transport(String),
}

impl std::fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Status(status) => write!(f, "upstream returned {status}"),
            Self::RateLimited(_) => write!(f, "upstream rate limit reached"),
            Self::Transport(err) => write!(f, "{err}"),
        }
    }
}

impl From<reqwest::Error> for UpstreamError {
    fn from(err: reqwest::Error) -> Self {
        Self::Transport(err.to_string())
    }
}

async fn wait_for_flight(
    mut receiver: watch::Receiver<FlightState>,
    deadline: Instant,
) -> Result<Option<String>, UpstreamError> {
    match timeout_at(
        deadline,
        receiver.wait_for(|state| !matches!(state, FlightState::Pending)),
    )
    .await
    {
        Err(_) => Err(UpstreamError::Transport("token exchange wait timed out".to_owned())),
        Ok(Err(_)) => Ok(None),
        Ok(Ok(state)) => Ok(match &*state {
            FlightState::Ready(token) => Some(token.clone()),
            FlightState::Pending | FlightState::Failed => None,
        }),
    }
}

impl From<serde_json::Error> for UpstreamError {
    fn from(err: serde_json::Error) -> Self {
        Self::Transport(err.to_string())
    }
}

impl From<CredentialError> for UpstreamError {
    fn from(error: CredentialError) -> Self {
        Self::Transport(error.to_string())
    }
}

impl Default for Upstream {
    fn default() -> Self {
        Self::new()
    }
}

impl Upstream {
    /// # Panics
    /// Panics only if the TLS backend cannot initialize the HTTP client, which cannot happen once the
    /// ring crypto provider is installed.
    #[must_use]
    pub fn new() -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .user_agent(concat!("peryx/", env!("CARGO_PKG_VERSION")))
            .pool_max_idle_per_host(32)
            .http2_adaptive_window(true)
            .build()
            .expect("build the OCI upstream HTTP client");
        Self {
            http,
            tokens: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashMap::new()),
            token_flight_timeout: TOKEN_FLIGHT_TIMEOUT,
        }
    }

    /// Fetch a manifest from `base` for `repo`/`reference`, returning the raw response so the caller
    /// reads its digest header, content type, and body. Always a `GET`: a served `HEAD` still needs the
    /// body to cache, so the driver reads it here and drops it on the way out.
    ///
    /// # Errors
    /// Returns [`UpstreamError`] on a non-success status or a transport failure.
    pub async fn manifest(
        &self,
        base: &str,
        credentials: &CredentialProvider,
        repo: &str,
        reference: &str,
    ) -> Result<Response, UpstreamError> {
        let url = format!("{base}v2/{repo}/manifests/{reference}");
        self.send(Method::GET, base, credentials, &url, repo, Some(ACCEPT_MANIFESTS))
            .await
    }

    /// Ask what a tag points at, without asking for what it points at.
    ///
    /// A `HEAD` on a manifest answers with `Docker-Content-Digest` and no body, so a revalidation of
    /// an unchanged tag costs a round trip instead of the manifest. `None` means the upstream did not
    /// name a digest, and the caller must fetch to find out.
    ///
    /// # Errors
    /// Returns [`UpstreamError`] on a non-success status or a transport failure.
    pub async fn manifest_digest(
        &self,
        base: &str,
        credentials: &CredentialProvider,
        repo: &str,
        reference: &str,
    ) -> Result<Option<String>, UpstreamError> {
        let url = format!("{base}v2/{repo}/manifests/{reference}");
        let response = self
            .send(Method::HEAD, base, credentials, &url, repo, Some(ACCEPT_MANIFESTS))
            .await?;
        Ok(response
            .headers()
            .get("docker-content-digest")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned))
    }

    /// # Errors
    /// Returns [`UpstreamError`] on a non-success status or a transport failure.
    pub async fn blob(
        &self,
        base: &str,
        credentials: &CredentialProvider,
        repo: &str,
        digest: &str,
    ) -> Result<Response, UpstreamError> {
        let url = format!("{base}v2/{repo}/blobs/{digest}");
        self.send(Method::GET, base, credentials, &url, repo, None).await
    }

    /// Check a blob's existence and size with a `HEAD`, so a client's pre-flight `HEAD` need not pull
    /// the whole layer. Returns the `Content-Length` when the upstream provides one.
    ///
    /// # Errors
    /// Returns [`UpstreamError`] on a non-success status (a `404` means absent) or a transport failure.
    pub async fn blob_head(
        &self,
        base: &str,
        credentials: &CredentialProvider,
        repo: &str,
        digest: &str,
    ) -> Result<Option<u64>, UpstreamError> {
        let url = format!("{base}v2/{repo}/blobs/{digest}");
        let response = self.send(Method::HEAD, base, credentials, &url, repo, None).await?;
        Ok(response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok()))
    }

    /// # Errors
    /// Returns [`UpstreamError`] on a non-success status or a transport failure.
    pub async fn referrers(
        &self,
        base: &str,
        credentials: &CredentialProvider,
        repo: &str,
        digest: &str,
    ) -> Result<Response, UpstreamError> {
        let url = format!("{base}v2/{repo}/referrers/{digest}");
        self.send(
            Method::GET,
            base,
            credentials,
            &url,
            repo,
            Some("application/vnd.oci.image.index.v1+json"),
        )
        .await
    }

    /// # Errors
    /// Returns [`UpstreamError`] on a non-success status or a transport failure.
    pub async fn tags(
        &self,
        base: &str,
        credentials: &CredentialProvider,
        repo: &str,
        query: &str,
    ) -> Result<Response, UpstreamError> {
        let mut url = format!("{base}v2/{repo}/tags/list");
        if !query.is_empty() {
            url.push('?');
            url.push_str(query);
        }
        self.send(Method::GET, base, credentials, &url, repo, None).await
    }

    /// Send `method` with the token-auth flow: attach a cached token if any, and on a `401` carrying a
    /// bearer challenge, trade the configured credentials for a fresh token, cache it, and replay once.
    /// The token cache is keyed by `(base, scope, provider)` and records the credential generation.
    /// One provider reuses a token until its credentials rotate; distinct providers cannot exchange
    /// tokens even when their current secret text matches. Credentials only reach the token realm,
    /// never the registry object endpoint or a blob CDN redirect.
    async fn send(
        &self,
        method: Method,
        base: &str,
        credentials: &CredentialProvider,
        url: &str,
        repo: &str,
        accept: Option<&str>,
    ) -> Result<Response, UpstreamError> {
        let scope = format!("repository:{repo}:pull");
        let credential = credentials.credential().await?;
        let cache_key = token_cache_key(base, &scope, credential.identity().provider());
        let cached = self.cached_token(&cache_key, &credential).await;
        let response = self.attempt(&method, url, accept, cached.as_deref()).await?;
        if response.status() != StatusCode::UNAUTHORIZED {
            return finish(response);
        }
        let Some(challenge) = response
            .headers()
            .get_all("www-authenticate")
            .iter()
            .find_map(parse_bearer)
        else {
            return finish(response);
        };
        let token = self
            .acquire_token(&cache_key, cached.as_deref(), &challenge, credentials, &credential)
            .await?;
        finish(self.attempt(&method, url, accept, Some(&token)).await?)
    }

    /// Single-flight the token exchange for `cache_key`. The first pull to miss a key inserts an
    /// in-flight slot and runs the exchange; concurrent pulls for the same key await that one result
    /// instead of each calling the token service. A failed leader clears the slot before it publishes,
    /// so the waiters it wakes re-enter this loop and elect one fresh leader to retry, never a herd.
    async fn acquire_token(
        &self,
        cache_key: &TokenCacheKey,
        rejected_token: Option<&str>,
        challenge: &Bearer,
        credentials: &CredentialProvider,
        credential: &Arc<CredentialSnapshot>,
    ) -> Result<String, UpstreamError> {
        let deadline = Instant::now() + self.token_flight_timeout;
        loop {
            let waiter = {
                let mut inflight = self.inflight.lock().await;
                if let Some(receiver) = inflight.get(cache_key) {
                    receiver.clone()
                } else {
                    if let Some(token) = self
                        .cached_token(cache_key, credential)
                        .await
                        .filter(|token| Some(token.as_str()) != rejected_token)
                    {
                        return Ok(token);
                    }
                    let (sender, receiver) = watch::channel(FlightState::Pending);
                    inflight.insert(cache_key.clone(), receiver);
                    drop(inflight);
                    let result = timeout_at(
                        deadline,
                        self.exchange(
                            &cache_key.base,
                            &cache_key.scope,
                            challenge,
                            credentials,
                            credential.clone(),
                        ),
                    )
                    .await
                    .map_err(|_| UpstreamError::Transport("token exchange timed out".to_owned()))
                    .and_then(|result| result);
                    self.inflight.lock().await.remove(cache_key);
                    let _ = sender.send(
                        result
                            .as_ref()
                            .map_or(FlightState::Failed, |token| FlightState::Ready(token.clone())),
                    );
                    return result;
                }
            };
            if let Some(token) = wait_for_flight(waiter, deadline).await? {
                return Ok(token);
            }
        }
    }

    async fn cached_token(&self, cache_key: &TokenCacheKey, credential: &CredentialSnapshot) -> Option<String> {
        self.tokens
            .lock()
            .await
            .get(cache_key)
            .filter(|token| token.credentials == credential.identity())
            .map(|token| token.value.clone())
    }

    /// Trade the bearer challenge for a token and cache it, refreshing the source credential once if
    /// the realm rejects the current generation. This is the unit `acquire_token` single-flights.
    async fn exchange(
        &self,
        base: &str,
        scope: &str,
        challenge: &Bearer,
        credentials: &CredentialProvider,
        mut credential: Arc<CredentialSnapshot>,
    ) -> Result<String, UpstreamError> {
        let mut auth = credential.auth();
        let token = match self.fetch_token(challenge, scope, auth).await {
            Err(UpstreamError::Status(StatusCode::UNAUTHORIZED)) => {
                let generation = credential.generation();
                credential = credentials.refresh_after_unauthorized(generation).await?;
                if credential.generation() == generation {
                    return Err(UpstreamError::Status(StatusCode::UNAUTHORIZED));
                }
                auth = credential.auth();
                self.fetch_token(challenge, scope, auth).await?
            }
            result => result?,
        };
        self.tokens.lock().await.insert(
            token_cache_key(base, scope, credential.identity().provider()),
            CachedToken {
                credentials: credential.identity(),
                value: token.clone(),
            },
        );
        Ok(token)
    }

    async fn attempt(
        &self,
        method: &Method,
        url: &str,
        accept: Option<&str>,
        token: Option<&str>,
    ) -> Result<Response, UpstreamError> {
        let mut request = self.http.request(method.clone(), url);
        if let Some(accept) = accept {
            request = request.header("accept", accept);
        }
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        Ok(request.send().await?)
    }

    /// Trade a bearer challenge for a token at its realm, presenting the configured credentials so the
    /// realm returns an authenticated token (Docker Hub's higher rate tier) rather than an anonymous one.
    async fn fetch_token(&self, challenge: &Bearer, scope: &str, auth: &Auth) -> Result<String, UpstreamError> {
        let scope = challenge.scope.as_deref().unwrap_or(scope);
        let mut url = url::Url::parse(&challenge.realm)
            .map_err(|err| UpstreamError::Transport(format!("invalid bearer realm: {err}")))?;
        if let Auth::Basic { .. } = auth
            && url.scheme() != "https"
            && !realm_host_is_loopback(&url)
        {
            return Err(UpstreamError::Transport(format!(
                "insecure bearer realm {}: refusing to send Basic credentials over cleartext",
                challenge.realm
            )));
        }
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("scope", scope);
            if let Some(service) = &challenge.service {
                query.append_pair("service", service);
            }
        }
        let mut request = self.http.get(url);
        if let Auth::Basic { username, password } = auth {
            request = request.basic_auth(username, Some(password));
        }
        let response = request.send().await?;
        if !response.status().is_success() {
            return Err(UpstreamError::Status(response.status()));
        }
        let body: TokenResponse = serde_json::from_slice(&read_capped(response).await?)?;
        body.token
            .or(body.access_token)
            .ok_or_else(|| UpstreamError::Transport("token endpoint returned no token".to_owned()))
    }
}

/// The largest token-endpoint response the client reads. A bearer token runs a few hundred bytes, so
/// this cap is generous while it stops an unbounded or hostile auth response from exhausting memory.
const MAX_TOKEN_RESPONSE_BYTES: u64 = 1 << 20;

/// Read `response` into a buffer, refusing a body past [`MAX_TOKEN_RESPONSE_BYTES`] before it retains
/// another chunk. The realm is untrusted, so the read is bounded without trusting an advertised length:
/// a peer that streams an unbounded body is rejected before the buffer grows past the cap.
async fn read_capped(mut response: Response) -> Result<Vec<u8>, UpstreamError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() as u64 + chunk.len() as u64 > MAX_TOKEN_RESPONSE_BYTES {
            return Err(UpstreamError::Transport(format!(
                "upstream token response exceeds the {MAX_TOKEN_RESPONSE_BYTES}-byte limit"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Whether the realm host is a loopback address, the one `http` case where Basic credentials stay on
/// the machine rather than going out in cleartext. A local or dev registry served over `http` on
/// `localhost` keeps working; only a routable `http` realm is refused.
fn realm_host_is_loopback(url: &url::Url) -> bool {
    url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost") || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
    })
}

fn token_cache_key(base: &str, scope: &str, provider: CredentialProviderId) -> TokenCacheKey {
    TokenCacheKey {
        base: base.to_owned(),
        scope: scope.to_owned(),
        provider,
    }
}

/// Fail a non-success response, otherwise hand it back for the caller to read. A `429` becomes a
/// [`UpstreamError::RateLimited`] carrying the upstream's `Retry-After`, so the client is told to back
/// off rather than seeing an opaque gateway error.
fn finish(response: Response) -> Result<Response, UpstreamError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        return Err(UpstreamError::RateLimited(retry_after));
    }
    Err(UpstreamError::Status(status))
}

/// A parsed `WWW-Authenticate: Bearer` challenge.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Bearer {
    realm: String,
    service: Option<String>,
    scope: Option<String>,
}

fn parse_bearer(value: &HeaderValue) -> Option<Bearer> {
    let mut value = value.to_str().ok()?;
    loop {
        if let Some(parameters) = strip_auth_scheme(trim_ows(value), "Bearer")
            && let Some(challenge) = parse_bearer_parameters(parameters)
        {
            return Some(challenge);
        }
        let comma = next_comma(value)?;
        value = &value[comma + 1..];
    }
}

fn next_comma(value: &str) -> Option<usize> {
    let mut quoted = false;
    let mut escaped = false;
    for (index, byte) in value.bytes().enumerate() {
        if quoted {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                quoted = false;
            }
        } else if byte == b'"' {
            quoted = true;
        } else if byte == b',' {
            return Some(index);
        }
    }
    None
}

fn parse_bearer_parameters(mut rest: &str) -> Option<Bearer> {
    let mut realm = None;
    let mut service = None;
    let mut scope = None;
    let mut extension_names: Vec<&str> = Vec::new();
    loop {
        if !starts_parameter(rest) {
            break;
        }
        let (key, value, tail) = auth_parameter(rest)?;
        if key.eq_ignore_ascii_case("realm") {
            if realm.is_some() {
                return None;
            }
            realm = Some(value.into_owned());
        } else if key.eq_ignore_ascii_case("service") {
            if service.is_some() {
                return None;
            }
            service = Some(value.into_owned());
        } else if key.eq_ignore_ascii_case("scope") {
            if scope.is_some() {
                return None;
            }
            scope = Some(value.into_owned());
        } else {
            if extension_names.iter().any(|name| key.eq_ignore_ascii_case(name)) {
                return None;
            }
            extension_names.push(key);
        }
        rest = trim_ows(tail);
        if rest.is_empty() {
            break;
        }
        rest = trim_ows(rest.strip_prefix(',')?);
        if rest.is_empty() {
            break;
        }
    }
    Some(Bearer {
        realm: realm.filter(|value| !value.is_empty())?,
        service,
        scope,
    })
}

fn starts_parameter(value: &str) -> bool {
    let value = trim_ows(value);
    let key_len = token_len(value);
    key_len > 0 && trim_ows(&value[key_len..]).starts_with('=')
}

fn token_len(value: &str) -> usize {
    value.bytes().position(|byte| !is_token(byte)).unwrap_or(value.len())
}

fn auth_parameter(value: &str) -> Option<(&str, Cow<'_, str>, &str)> {
    let value = trim_ows(value);
    let key_len = token_len(value);
    let key = &value[..key_len];
    let rest = trim_ows(&value[key_len..]).strip_prefix('=')?;
    let (value, rest) = auth_value(trim_ows(rest))?;
    Some((key, value, rest))
}

fn auth_value(value: &str) -> Option<(Cow<'_, str>, &str)> {
    if let Some(value) = value.strip_prefix('"') {
        for (index, byte) in value.bytes().enumerate() {
            match byte {
                b'"' => return Some((Cow::Borrowed(&value[..index]), &value[index + 1..])),
                b'\\' => return unescape_quoted(value, index),
                _ => {}
            }
        }
        return None;
    }
    let len = value.bytes().position(|byte| !is_token(byte)).unwrap_or(value.len());
    (len > 0).then(|| (Cow::Borrowed(&value[..len]), &value[len..]))
}

fn unescape_quoted(value: &str, first_escape: usize) -> Option<(Cow<'_, str>, &str)> {
    let bytes = value.as_bytes();
    let mut decoded = String::with_capacity(value.len());
    decoded.push_str(&value[..first_escape]);
    let mut index = first_escape;
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'"' => return Some((Cow::Owned(decoded), &value[index + 1..])),
            b'\\' => {
                index += 1;
                let escaped = *bytes.get(index).filter(|&&byte| is_quoted_pair(byte))?;
                decoded.push(char::from(escaped));
            }
            _ => decoded.push(char::from(byte)),
        }
        index += 1;
    }
    None
}

fn trim_ows(value: &str) -> &str {
    value.trim_matches(|char| matches!(char, ' ' | '\t'))
}

const fn is_token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
        )
}

const fn is_quoted_pair(byte: u8) -> bool {
    matches!(byte, b'\t' | b' ' | b'!'..=b'~')
}

/// The token endpoint's JSON: registries return `token`, some return `access_token`.
#[derive(Debug, serde::Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
}

#[cfg(test)]
#[path = "../tests/unit/upstream/tests.rs"]
mod tests;
