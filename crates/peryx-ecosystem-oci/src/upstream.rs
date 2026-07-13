//! The read-through client for an upstream OCI registry.
//!
//! Pulls speak the distribution-spec pull API with the token-auth flow real registries require: an
//! anonymous request draws a `401` carrying `WWW-Authenticate: Bearer realm=…,service=…,scope=…`, the
//! client trades that challenge for a bearer token at the realm, then replays the request. Tokens are
//! cached per scope so a burst of blob pulls authenticates once, and a cached token that has expired
//! (a late `401`) re-runs the flow transparently.

use std::collections::HashMap;

use axum::http::{HeaderValue, Method, StatusCode};
use peryx_identity::strip_auth_scheme;
use peryx_upstream::Auth;
use reqwest::Response;
use tokio::sync::Mutex;

/// The manifest media types a puller accepts, mirroring containerd/docker: the Docker v2 schema and
/// manifest list, the OCI image manifest and index, then `*/*` so a registry that only knows one of
/// them still answers.
const ACCEPT_MANIFESTS: &str = "application/vnd.docker.distribution.manifest.v2+json, \
application/vnd.docker.distribution.manifest.list.v2+json, \
application/vnd.oci.image.manifest.v1+json, \
application/vnd.oci.image.index.v1+json, \
*/*";

/// A shared upstream fetcher: one HTTP client and one token cache for every configured OCI proxy.
#[derive(Debug)]
pub struct Upstream {
    http: reqwest::Client,
    tokens: Mutex<HashMap<String, String>>,
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

impl From<serde_json::Error> for UpstreamError {
    fn from(err: serde_json::Error) -> Self {
        Self::Transport(err.to_string())
    }
}

impl Default for Upstream {
    fn default() -> Self {
        Self::new()
    }
}

impl Upstream {
    /// Build the shared upstream client. The HTTP client is created once and reused across proxies.
    ///
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
        auth: &Auth,
        repo: &str,
        reference: &str,
    ) -> Result<Response, UpstreamError> {
        let url = format!("{base}v2/{repo}/manifests/{reference}");
        self.send(Method::GET, base, auth, &url, repo, Some(ACCEPT_MANIFESTS))
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
        auth: &Auth,
        repo: &str,
        reference: &str,
    ) -> Result<Option<String>, UpstreamError> {
        let url = format!("{base}v2/{repo}/manifests/{reference}");
        let response = self
            .send(Method::HEAD, base, auth, &url, repo, Some(ACCEPT_MANIFESTS))
            .await?;
        Ok(response
            .headers()
            .get("docker-content-digest")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned))
    }

    /// Fetch a blob from `base` for `repo`/`digest`, returning the raw response for streaming.
    ///
    /// # Errors
    /// Returns [`UpstreamError`] on a non-success status or a transport failure.
    pub async fn blob(&self, base: &str, auth: &Auth, repo: &str, digest: &str) -> Result<Response, UpstreamError> {
        let url = format!("{base}v2/{repo}/blobs/{digest}");
        self.send(Method::GET, base, auth, &url, repo, None).await
    }

    /// Check a blob's existence and size with a `HEAD`, so a client's pre-flight `HEAD` need not pull
    /// the whole layer. Returns the `Content-Length`, or `0` when the upstream omits it.
    ///
    /// # Errors
    /// Returns [`UpstreamError`] on a non-success status (a `404` means absent) or a transport failure.
    pub async fn blob_head(&self, base: &str, auth: &Auth, repo: &str, digest: &str) -> Result<u64, UpstreamError> {
        let url = format!("{base}v2/{repo}/blobs/{digest}");
        let response = self.send(Method::HEAD, base, auth, &url, repo, None).await?;
        Ok(response
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse().ok())
            .unwrap_or(0))
    }

    /// Fetch the referrers index for `repo`/`digest`: the manifests upstream records as declaring the
    /// digest their subject (signatures, SBOMs, attestations).
    ///
    /// # Errors
    /// Returns [`UpstreamError`] on a non-success status or a transport failure.
    pub async fn referrers(
        &self,
        base: &str,
        auth: &Auth,
        repo: &str,
        digest: &str,
    ) -> Result<Response, UpstreamError> {
        let url = format!("{base}v2/{repo}/referrers/{digest}");
        self.send(
            Method::GET,
            base,
            auth,
            &url,
            repo,
            Some("application/vnd.oci.image.index.v1+json"),
        )
        .await
    }

    /// Fetch the raw `tags/list` response for `repo`, forwarding the client's pagination query.
    ///
    /// # Errors
    /// Returns [`UpstreamError`] on a non-success status or a transport failure.
    pub async fn tags(&self, base: &str, auth: &Auth, repo: &str, query: &str) -> Result<Response, UpstreamError> {
        let mut url = format!("{base}v2/{repo}/tags/list");
        if !query.is_empty() {
            url.push('?');
            url.push_str(query);
        }
        self.send(Method::GET, base, auth, &url, repo, None).await
    }

    /// Send `method` with the token-auth flow: attach a cached token if any, and on a `401` carrying a
    /// bearer challenge, trade the configured credentials for a fresh token, cache it, and replay once.
    /// The token cache is keyed by `(base, scope, identity)`, so once one object in a repo authenticates
    /// the rest reuse the token; the credentials only ever reach the realm, never the object (or a blob
    /// CDN it redirects to). The identity keys the cache by *whose* credentials minted the token: one
    /// shared `Upstream` serves every proxy, so without it two indexes on the same host with different
    /// credentials would trade tokens — an anonymous index riding a private token, or vice versa.
    async fn send(
        &self,
        method: Method,
        base: &str,
        auth: &Auth,
        url: &str,
        repo: &str,
        accept: Option<&str>,
    ) -> Result<Response, UpstreamError> {
        let scope = format!("repository:{repo}:pull");
        let cache_key = format!("{base}\u{0}{scope}\u{0}{}", auth_identity(auth));
        let cached = self.tokens.lock().await.get(&cache_key).cloned();
        let response = self.attempt(&method, url, accept, cached.as_deref()).await?;
        if response.status() != StatusCode::UNAUTHORIZED {
            return finish(response);
        }
        let Some(challenge) = response.headers().get("www-authenticate").and_then(parse_bearer) else {
            return finish(response);
        };
        let token = self.fetch_token(&challenge, &scope, auth).await?;
        self.tokens.lock().await.insert(cache_key, token.clone());
        finish(self.attempt(&method, url, accept, Some(&token)).await?)
    }

    /// One attempt with the given method, optionally bearing a token and an `Accept` header.
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
        let mut url = format!("{}?scope={}", challenge.realm, encode_query(scope));
        if let Some(service) = &challenge.service {
            url.push_str("&service=");
            url.push_str(&encode_query(service));
        }
        let mut request = self.http.get(&url);
        if let Auth::Basic { username, password } = auth {
            request = request.basic_auth(username, Some(password));
        }
        let response = request.send().await?;
        if !response.status().is_success() {
            return Err(UpstreamError::Status(response.status()));
        }
        let body: TokenResponse = serde_json::from_str(&response.text().await?)?;
        body.token
            .or(body.access_token)
            .ok_or_else(|| UpstreamError::Transport("token endpoint returned no token".to_owned()))
    }
}

/// A stable, opaque identity for the credentials that mint a token, for use in the token cache key.
/// Distinct credentials must never collide, so `Basic`/`Bearer` fold their secret into a hash rather
/// than the raw value — the key can end up in a log or a panic, and a password must not.
fn auth_identity(auth: &Auth) -> String {
    match auth {
        Auth::None => "none".to_owned(),
        Auth::Basic { username, password } => format!("basic:{:016x}", hash_secret(&[username, password])),
        Auth::Bearer(token) => format!("bearer:{:016x}", hash_secret(&[token])),
    }
}

/// Hash secret parts length-delimited (so `["ab","c"]` and `["a","bc"]` differ), never logging them.
fn hash_secret(parts: &[&str]) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::hash::DefaultHasher::new();
    for part in parts {
        part.hash(&mut hasher);
    }
    hasher.finish()
}

/// Percent-encode a query-parameter value: the token scope and service names contain `:` and `/`.
fn encode_query(value: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
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

/// Parse a `WWW-Authenticate` header value, keeping only a `Bearer` challenge with a realm.
fn parse_bearer(value: &HeaderValue) -> Option<Bearer> {
    let value = value.to_str().ok()?;
    let rest = strip_auth_scheme(value, "Bearer")?;
    let mut realm = None;
    let mut service = None;
    let mut scope = None;
    for parameter in rest.split(',') {
        let (key, raw) = parameter.trim().split_once('=')?;
        let unquoted = raw.trim().trim_matches('"').to_owned();
        match key.trim() {
            "realm" => realm = Some(unquoted),
            "service" => service = Some(unquoted),
            "scope" => scope = Some(unquoted),
            _ => {}
        }
    }
    Some(Bearer {
        realm: realm?,
        service,
        scope,
    })
}

/// The token endpoint's JSON: registries return `token`, some return `access_token`.
#[derive(Debug, serde::Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(value: &str) -> HeaderValue {
        HeaderValue::from_str(value).unwrap()
    }

    #[test]
    fn test_parse_bearer_reads_realm_service_scope() {
        let challenge = parse_bearer(&header(
            r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/nginx:pull""#,
        ))
        .unwrap();
        assert_eq!(challenge.realm, "https://auth.docker.io/token");
        assert_eq!(challenge.service.as_deref(), Some("registry.docker.io"));
        assert_eq!(challenge.scope.as_deref(), Some("repository:library/nginx:pull"));
    }

    #[test]
    fn test_parse_bearer_realm_only() {
        let challenge = parse_bearer(&header(r#"bearer realm="https://auth.example/token""#)).unwrap();
        assert_eq!(challenge.realm, "https://auth.example/token");
        assert_eq!(challenge.service, None);
        assert_eq!(challenge.scope, None);
    }

    #[test]
    fn test_parse_bearer_rejects_non_bearer_scheme() {
        assert_eq!(parse_bearer(&header(r#"Basic realm="x""#)), None);
    }

    #[test]
    fn test_parse_bearer_requires_a_realm() {
        assert_eq!(parse_bearer(&header(r#"Bearer service="registry.docker.io""#)), None);
    }

    #[test]
    fn test_parse_bearer_rejects_malformed_parameter() {
        assert_eq!(parse_bearer(&header("Bearer realmnoeq")), None);
    }

    #[test]
    fn test_parse_bearer_ignores_unknown_parameters() {
        let challenge = parse_bearer(&header(
            r#"Bearer realm="https://auth.example/token",error="insufficient_scope""#,
        ))
        .unwrap();
        assert_eq!(challenge.realm, "https://auth.example/token");
    }

    #[test]
    fn test_upstream_error_display() {
        assert_eq!(
            UpstreamError::Status(StatusCode::NOT_FOUND).to_string(),
            "upstream returned 404 Not Found"
        );
        assert_eq!(UpstreamError::Transport("reset".to_owned()).to_string(), "reset");
        assert_eq!(
            UpstreamError::RateLimited(Some("5".to_owned())).to_string(),
            "upstream rate limit reached"
        );
    }

    fn basic(username: &str, password: &str) -> Auth {
        Auth::Basic {
            username: username.to_owned(),
            password: password.to_owned(),
        }
    }

    #[test]
    fn test_auth_identity_distinguishes_credentials() {
        let anon = auth_identity(&Auth::None);
        let alice = auth_identity(&basic("alice", "pw"));
        assert_eq!(anon, "none");
        assert_ne!(anon, alice);
        assert_ne!(alice, auth_identity(&basic("bob", "pw")));
        assert_ne!(alice, auth_identity(&basic("alice", "other")));
        assert_ne!(auth_identity(&Auth::Bearer("t".to_owned())), anon);
        assert_ne!(auth_identity(&Auth::Bearer("t".to_owned())), alice);
        assert_eq!(alice, auth_identity(&basic("alice", "pw")));
    }

    use base64::Engine as _;
    use wiremock::matchers::{header as match_header, method, path};
    use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

    /// Match only the token-less first attempt, so the bearer-carrying retry falls through to the 200.
    struct Unauthenticated;
    impl Match for Unauthenticated {
        fn matches(&self, request: &Request) -> bool {
            !request.headers.contains_key("authorization")
        }
    }

    fn challenge(base: &str) -> ResponseTemplate {
        ResponseTemplate::new(401).insert_header(
            "www-authenticate",
            format!(r#"Bearer realm="{base}token",service="reg",scope="repository:library/nginx:pull""#).as_str(),
        )
    }

    #[tokio::test]
    async fn test_send_does_not_share_a_token_across_credentials() {
        let server = MockServer::start().await;
        let base = format!("{}/", server.uri());
        let alice = base64::engine::general_purpose::STANDARD.encode("alice:pw1");
        let bob = base64::engine::general_purpose::STANDARD.encode("bob:pw2");
        Mock::given(method("GET"))
            .and(path("/v2/library/nginx/manifests/latest"))
            .and(Unauthenticated)
            .respond_with(challenge(&base))
            .mount(&server)
            .await;
        for (auth_header, token) in [(&alice, "tok-alice"), (&bob, "tok-bob")] {
            Mock::given(method("GET"))
                .and(path("/token"))
                .and(match_header("authorization", format!("Basic {auth_header}").as_str()))
                .respond_with(ResponseTemplate::new(200).set_body_string(format!(r#"{{"token":"{token}"}}"#)))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("GET"))
                .and(path("/v2/library/nginx/manifests/latest"))
                .and(match_header("authorization", format!("Bearer {token}").as_str()))
                .respond_with(ResponseTemplate::new(200))
                .expect(1)
                .mount(&server)
                .await;
        }

        let upstream = Upstream::new();
        upstream
            .manifest(&base, &basic("alice", "pw1"), "library/nginx", "latest")
            .await
            .unwrap();
        upstream
            .manifest(&base, &basic("bob", "pw2"), "library/nginx", "latest")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn test_send_reuses_a_cached_token_for_the_same_credentials() {
        let server = MockServer::start().await;
        let base = format!("{}/", server.uri());
        Mock::given(method("GET"))
            .and(path("/v2/library/nginx/manifests/latest"))
            .and(Unauthenticated)
            .respond_with(challenge(&base))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"token":"tok"}"#))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v2/library/nginx/manifests/latest"))
            .and(match_header("authorization", "Bearer tok"))
            .respond_with(ResponseTemplate::new(200))
            .expect(2)
            .mount(&server)
            .await;

        let upstream = Upstream::new();
        let auth = basic("alice", "pw1");
        upstream
            .manifest(&base, &auth, "library/nginx", "latest")
            .await
            .unwrap();
        upstream
            .manifest(&base, &auth, "library/nginx", "latest")
            .await
            .unwrap();
    }
}
