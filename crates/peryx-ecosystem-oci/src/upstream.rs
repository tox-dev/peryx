//! Pulls speak the distribution-spec pull API with the token-auth flow real registries require: an
//! anonymous request draws a `401` carrying `WWW-Authenticate: Bearer realm=…,service=…,scope=…`, the
//! client trades that challenge for a bearer token at the realm, then replays the request. Tokens are
//! cached per scope so a burst of blob pulls authenticates once, and a cached token that has expired
//! (a late `401`) re-runs the flow transparently.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::http::{HeaderValue, Method, StatusCode};
use peryx_identity::strip_auth_scheme;
use peryx_upstream::{
    Auth, CredentialError, CredentialIdentity, CredentialProvider, CredentialProviderId, CredentialSnapshot,
    UpstreamClient,
};
use reqwest::Response;
use tokio::sync::{Mutex, broadcast};
use tokio::time::{Instant, timeout_at};

use crate::realm::TokenRealms;

const TOKEN_FLIGHT_TIMEOUT: Duration = Duration::from_secs(30);

/// The manifest media types a puller accepts, mirroring containerd/docker: the Docker v2 schema and
/// manifest list, the OCI image manifest and index, then `*/*` so a registry that only knows one of
/// them still answers.
const ACCEPT_MANIFESTS: &str = "application/vnd.docker.distribution.manifest.v2+json, \
application/vnd.docker.distribution.manifest.list.v2+json, \
application/vnd.oci.image.manifest.v1+json, \
application/vnd.oci.image.index.v1+json, \
*/*";

/// A shared token cache for every configured OCI proxy. Callers pass the selected index's guarded client.
///
/// `inflight` single-flights token exchanges: concurrent cold pulls that miss the same `(base, scope,
/// provider)` key elect one leader to trade the challenge for a token while the rest await its result,
/// so a burst that would otherwise fire one token request per pull fires one for the whole burst.
#[derive(Debug)]
pub struct Upstream {
    tokens: Mutex<HashMap<TokenCacheKey, CachedToken>>,
    inflight: Mutex<HashMap<TokenCacheKey, broadcast::Sender<String>>>,
    token_flight_timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TokenCacheKey {
    base: String,
    scope: String,
    provider: CredentialProviderId,
}

/// What one token exchange needs beyond its cache key: the challenge to trade, the provider that can
/// replace a rejected credential, the snapshot in hand, and the realms it may be shown to.
struct TokenExchange<'a> {
    challenge: &'a Bearer,
    credentials: &'a CredentialProvider,
    credential: &'a Arc<CredentialSnapshot>,
    realms: &'a TokenRealms,
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
        // reqwest's display text omits the custom redirect cause that identifies the blocked address.
        let mut source: Option<&(dyn std::error::Error + 'static)> = Some(&err);
        while let Some(error) = source {
            if let Some(error @ peryx_upstream::UpstreamError::BlockedDestination { .. }) =
                error.downcast_ref::<peryx_upstream::UpstreamError>()
            {
                return Self::Transport(error.to_string());
            }
            source = error.source();
        }
        Self::Transport(err.to_string())
    }
}

impl From<peryx_upstream::UpstreamError> for UpstreamError {
    fn from(error: peryx_upstream::UpstreamError) -> Self {
        Self::Transport(error.to_string())
    }
}

async fn wait_for_flight(
    mut receiver: broadcast::Receiver<String>,
    deadline: Instant,
) -> Result<Option<String>, UpstreamError> {
    match timeout_at(deadline, receiver.recv()).await {
        Err(_) => Err(UpstreamError::Transport("token exchange wait timed out".to_owned())),
        Ok(Err(_)) => Ok(None),
        Ok(Ok(token)) => Ok(Some(token)),
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
    #[must_use]
    pub fn new() -> Self {
        Self {
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
        client: &UpstreamClient,
        repo: &str,
        reference: &str,
        realms: &TokenRealms,
    ) -> Result<Response, UpstreamError> {
        let url = format!("{}v2/{repo}/manifests/{reference}", client.base_url());
        self.send(Method::GET, client, &url, repo, Some(ACCEPT_MANIFESTS), realms)
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
        client: &UpstreamClient,
        repo: &str,
        reference: &str,
        realms: &TokenRealms,
    ) -> Result<Option<String>, UpstreamError> {
        let url = format!("{}v2/{repo}/manifests/{reference}", client.base_url());
        let response = self
            .send(Method::HEAD, client, &url, repo, Some(ACCEPT_MANIFESTS), realms)
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
        client: &UpstreamClient,
        repo: &str,
        digest: &str,
        realms: &TokenRealms,
    ) -> Result<Response, UpstreamError> {
        let url = format!("{}v2/{repo}/blobs/{digest}", client.base_url());
        self.send(Method::GET, client, &url, repo, None, realms).await
    }

    /// Check a blob's existence and size with a `HEAD`, so a client's pre-flight `HEAD` need not pull
    /// the whole layer. Returns the `Content-Length` when the upstream provides one.
    ///
    /// # Errors
    /// Returns [`UpstreamError`] on a non-success status (a `404` means absent) or a transport failure.
    pub async fn blob_head(
        &self,
        client: &UpstreamClient,
        repo: &str,
        digest: &str,
        realms: &TokenRealms,
    ) -> Result<Option<u64>, UpstreamError> {
        let url = format!("{}v2/{repo}/blobs/{digest}", client.base_url());
        let response = self.send(Method::HEAD, client, &url, repo, None, realms).await?;
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
        client: &UpstreamClient,
        repo: &str,
        digest: &str,
        realms: &TokenRealms,
    ) -> Result<Response, UpstreamError> {
        let url = format!("{}v2/{repo}/referrers/{digest}", client.base_url());
        self.send(
            Method::GET,
            client,
            &url,
            repo,
            Some("application/vnd.oci.image.index.v1+json"),
            realms,
        )
        .await
    }

    /// # Errors
    /// Returns [`UpstreamError`] on a non-success status or a transport failure.
    pub async fn tags(
        &self,
        client: &UpstreamClient,
        repo: &str,
        query: &str,
        realms: &TokenRealms,
    ) -> Result<Response, UpstreamError> {
        let mut url = format!("{}v2/{repo}/tags/list", client.base_url());
        if !query.is_empty() {
            url.push('?');
            url.push_str(query);
        }
        self.send(Method::GET, client, &url, repo, None, realms).await
    }

    /// Send `method` with the token-auth flow: attach a cached token if any, and on a `401` carrying a
    /// bearer challenge, trade the configured credentials for a fresh token, cache it, and replay once.
    /// The token cache is keyed by `(base, scope, provider)` and records the credential generation.
    /// One provider reuses a token until its credentials rotate; distinct providers cannot exchange
    /// tokens even when their current secret text matches. Credentials only reach a token realm the
    /// operator trusts, never the registry object endpoint or a blob CDN redirect.
    async fn send(
        &self,
        method: Method,
        client: &UpstreamClient,
        url: &str,
        repo: &str,
        accept: Option<&str>,
        realms: &TokenRealms,
    ) -> Result<Response, UpstreamError> {
        let scope = format!("repository:{repo}:pull");
        let credentials = client.auth();
        let credential = credentials.credential().await?;
        let cache_key = token_cache_key(client.base_url(), &scope, credential.identity().provider());
        let cached = self.cached_token(&cache_key, &credential).await;
        let response = self.attempt(client, &method, url, accept, cached.as_deref()).await?;
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
        let exchange = TokenExchange {
            challenge: &challenge,
            credentials,
            credential: &credential,
            realms,
        };
        let token = self
            .acquire_token(client, &cache_key, cached.as_deref(), &exchange)
            .await?;
        finish(self.attempt(client, &method, url, accept, Some(&token)).await?)
    }

    /// Single-flight the token exchange for `cache_key`. The first pull to miss a key inserts an
    /// in-flight slot and runs the exchange; concurrent pulls for the same key await that one result
    /// instead of each calling the token service. A failed leader clears the slot before it publishes,
    /// so the waiters it wakes re-enter this loop and elect one fresh leader to retry, never a herd.
    async fn acquire_token(
        &self,
        client: &UpstreamClient,
        cache_key: &TokenCacheKey,
        rejected_token: Option<&str>,
        exchange: &TokenExchange<'_>,
    ) -> Result<String, UpstreamError> {
        let deadline = Instant::now() + self.token_flight_timeout;
        loop {
            let waiter = {
                let mut inflight = self.inflight.lock().await;
                if let Some(sender) = inflight.get(cache_key) {
                    sender.subscribe()
                } else {
                    if let Some(token) = self
                        .cached_token(cache_key, exchange.credential)
                        .await
                        .filter(|token| Some(token.as_str()) != rejected_token)
                    {
                        return Ok(token);
                    }
                    let (sender, _) = broadcast::channel(1);
                    inflight.insert(cache_key.clone(), sender.clone());
                    drop(inflight);
                    let result = timeout_at(deadline, self.exchange(client, &cache_key.scope, exchange))
                        .await
                        .map_err(|_| UpstreamError::Transport("token exchange timed out".to_owned()))
                        .and_then(|result| result);
                    self.inflight.lock().await.remove(cache_key);
                    if let Ok(token) = &result {
                        let _ = sender.send(token.clone());
                    }
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
        client: &UpstreamClient,
        scope: &str,
        exchange: &TokenExchange<'_>,
    ) -> Result<String, UpstreamError> {
        let mut credential = Arc::clone(exchange.credential);
        let mut auth = credential.auth();
        let token = match self
            .fetch_token(client, exchange.challenge, scope, auth, exchange.realms)
            .await
        {
            Err(UpstreamError::Status(StatusCode::UNAUTHORIZED)) => {
                let generation = credential.generation();
                credential = exchange.credentials.refresh_after_unauthorized(generation).await?;
                if credential.generation() == generation {
                    return Err(UpstreamError::Status(StatusCode::UNAUTHORIZED));
                }
                auth = credential.auth();
                self.fetch_token(client, exchange.challenge, scope, auth, exchange.realms)
                    .await?
            }
            result => result?,
        };
        self.tokens.lock().await.insert(
            token_cache_key(client.base_url(), scope, credential.identity().provider()),
            CachedToken {
                credentials: credential.identity(),
                value: token.clone(),
            },
        );
        Ok(token)
    }

    async fn attempt(
        &self,
        client: &UpstreamClient,
        method: &Method,
        url: &str,
        accept: Option<&str>,
        token: Option<&str>,
    ) -> Result<Response, UpstreamError> {
        let mut request = client.request_without_auth(method.clone(), url)?;
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
    ///
    /// The challenge picks the destination, so the credentials go only to an origin the operator
    /// configured: the upstream's own, or one named in `realms`. Any other realm is still contacted —
    /// a public registry issues anonymous pull tokens — but without the secret. Redirects are followed
    /// here rather than by the transport, so each hop is ruled on before it can receive one.
    async fn fetch_token(
        &self,
        client: &UpstreamClient,
        challenge: &Bearer,
        scope: &str,
        auth: &Auth,
        realms: &TokenRealms,
    ) -> Result<String, UpstreamError> {
        let scope = challenge.scope.as_deref().unwrap_or(scope);
        let mut url = url::Url::parse(&challenge.realm)
            .map_err(|err| UpstreamError::Transport(format!("invalid bearer realm: {err}")))?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("scope", scope);
            if let Some(service) = &challenge.service {
                query.append_pair("service", service);
            }
        }
        for _ in 0..=MAX_TOKEN_REALM_REDIRECTS {
            let mut request = client.request_without_auth_or_redirects(Method::GET, &url)?;
            let withheld = match auth {
                Auth::Basic { username, password } if realms.allows(client.base(), &url) => {
                    request = request.basic_auth(username, Some(password));
                    None
                }
                Auth::Basic { .. } => Some(url.origin().ascii_serialization()),
                Auth::None | Auth::Bearer(_) => None,
            };
            let response = request.send().await?;
            let status = response.status();
            if let Some(location) = redirect_target(&response) {
                url = url
                    .join(location)
                    .map_err(|err| UpstreamError::Transport(format!("invalid bearer realm redirect: {err}")))?;
                continue;
            }
            if status == StatusCode::UNAUTHORIZED
                && let Some(origin) = withheld
            {
                return Err(UpstreamError::Transport(format!(
                    "bearer realm {origin} is not a trusted token realm for this upstream, so the token \
                     request carried no credentials; add it to `token_realms` to authenticate there"
                )));
            }
            if !status.is_success() {
                return Err(UpstreamError::Status(status));
            }
            let body: TokenResponse = serde_json::from_slice(&read_capped(response).await?)?;
            return body
                .token
                .or(body.access_token)
                .ok_or_else(|| UpstreamError::Transport("token endpoint returned no token".to_owned()));
        }
        Err(UpstreamError::Transport(format!(
            "bearer realm redirected more than {MAX_TOKEN_REALM_REDIRECTS} times"
        )))
    }
}

/// How many redirects a token realm may take before the exchange gives up. A token endpoint answers
/// with its JSON directly; the allowance exists so a registry that fronts its authorization service
/// behind one still works, not so a challenge can walk the client around the network.
const MAX_TOKEN_REALM_REDIRECTS: usize = 3;

/// The `Location` of a redirect response, or `None` for any other response.
fn redirect_target(response: &Response) -> Option<&str> {
    if !response.status().is_redirection() {
        return None;
    }
    response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
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
