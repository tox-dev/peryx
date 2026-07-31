//! Browser OIDC login: the Authorization Code flow with PKCE.
//!
//! [`OidcLoginProvider`] runs the flow a browser drives, not the machine-to-machine token exchange in
//! [`crate::oidc`]. [`OidcLoginProvider::authorization`] mints the state, nonce, and PKCE verifier that
//! bind one login attempt and returns the provider redirect; [`OidcLoginProvider::callback`] exchanges
//! the returned code and validates the ID token before it trusts a subject. Discovery metadata and
//! signing keys are fetched once and cached with the configured issuer pinned, so a package request
//! after login never reaches the provider and a metadata outage cannot disable signature validation.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::jwk::{AlgorithmParameters, JwkSet, KeyOperations, PublicKeyUse};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use reqwest::header::{CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tokio::sync::Mutex as AsyncMutex;
use url::Url;

use crate::{
    ExternalGroup, ExternalGroupGrant, ExternalIdentity, ExternalIdentityLinker, ExternalIdentityResolution,
    ExternalIdentityStore, ExternalLogin, ExternalSubject, ProviderId, UserName,
};

const DISCOVERY_BODY_LIMIT: usize = 64 * 1024;
const JWKS_BODY_LIMIT: usize = 1024 * 1024;
const TOKEN_BODY_LIMIT: usize = 64 * 1024;
const MIN_FRESH_SECS: i64 = 60;
const DEFAULT_FRESH_SECS: i64 = 300;
const MAX_FRESH_SECS: i64 = 900;
const HARD_CACHE_SECS: i64 = 3600;
const RANDOM_BYTES: usize = 32;
const OPENID_SCOPE: &str = "openid";

/// Validated construction input for one browser OIDC login provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidcProviderSettings {
    pub id: ProviderId,
    /// The issuer URL, pinned: the discovery document must repeat it exactly.
    pub issuer: Url,
    pub client_id: String,
    /// The confidential client secret, or `None` for a public client that relies on PKCE alone.
    pub client_secret: Option<String>,
    /// The exact, pre-registered callback URL sent on both the authorization request and the exchange.
    pub redirect_uri: Url,
    /// The requested scopes; `openid` is required and inserted when absent.
    pub scopes: Vec<String>,
    /// The claim carrying the stable subject, conventionally `sub`.
    pub subject_claim: String,
    /// The claim carrying the display name, conventionally `name`.
    pub display_name_claim: String,
    /// The claim carrying group names, or `None` to assert no groups.
    pub groups_claim: Option<String>,
    /// Tolerance applied to `exp` and `iat` for provider clock drift.
    pub clock_skew: Duration,
    pub request_timeout: Duration,
}

/// A bounded Authorization Code client that returns provider assertions without changing local state.
#[derive(Clone)]
pub struct OidcLoginProvider {
    id: ProviderId,
    issuer: String,
    discovery: Url,
    client_id: String,
    client_secret: Option<String>,
    redirect_uri: Url,
    scope: String,
    subject_claim: String,
    display_name_claim: String,
    groups_claim: Option<String>,
    leeway_secs: u64,
    allow_insecure: bool,
    client: reqwest::Client,
    cache: Arc<AsyncMutex<Cache>>,
}

impl std::fmt::Debug for OidcLoginProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OidcLoginProvider")
            .field("id", &self.id)
            .field("issuer", &self.issuer)
            .field("client_id", &self.client_id)
            .field("client_secret", &self.client_secret.as_ref().map(|_| "[redacted]"))
            .field("redirect_uri", &self.redirect_uri)
            .field("scope", &self.scope)
            .field("subject_claim", &self.subject_claim)
            .field("display_name_claim", &self.display_name_claim)
            .field("groups_claim", &self.groups_claim)
            .finish_non_exhaustive()
    }
}

impl OidcLoginProvider {
    /// Build a lazy login client. Construction validates its inputs but opens no provider connection.
    ///
    /// # Errors
    /// Returns [`OidcProviderBuildError`] for an invalid issuer or redirect URL, an empty client ID or
    /// claim name, or a non-positive timeout.
    pub fn new(settings: OidcProviderSettings) -> Result<Self, OidcProviderBuildError> {
        Self::build(settings, false)
    }

    fn build(settings: OidcProviderSettings, allow_insecure: bool) -> Result<Self, OidcProviderBuildError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let issuer = secure_url(&settings.issuer, allow_insecure).ok_or(OidcProviderBuildError::InvalidIssuer)?;
        if settings.issuer.query().is_some() || settings.issuer.fragment().is_some() {
            return Err(OidcProviderBuildError::InvalidIssuer);
        }
        if secure_url(&settings.redirect_uri, allow_insecure).is_none() || settings.redirect_uri.fragment().is_some() {
            return Err(OidcProviderBuildError::InvalidRedirectUri);
        }
        if settings.client_id.is_empty() {
            return Err(OidcProviderBuildError::EmptyClientId);
        }
        if settings.subject_claim.is_empty() || settings.display_name_claim.is_empty() {
            return Err(OidcProviderBuildError::InvalidClaim);
        }
        if settings.groups_claim.as_ref().is_some_and(String::is_empty) {
            return Err(OidcProviderBuildError::InvalidClaim);
        }
        if settings.request_timeout.is_zero() {
            return Err(OidcProviderBuildError::InvalidTimeout);
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(settings.request_timeout)
            .build()
            .or(Err(OidcProviderBuildError::InvalidTimeout))?;
        Ok(Self {
            id: settings.id,
            issuer: settings.issuer.as_str().to_owned(),
            discovery: discovery_url(issuer),
            client_id: settings.client_id,
            client_secret: settings.client_secret,
            redirect_uri: settings.redirect_uri,
            scope: scope_string(&settings.scopes),
            subject_claim: settings.subject_claim,
            display_name_claim: settings.display_name_claim,
            groups_claim: settings.groups_claim,
            leeway_secs: settings.clock_skew.as_secs(),
            allow_insecure,
            client,
            cache: Arc::new(AsyncMutex::new(Cache::default())),
        })
    }

    #[must_use]
    pub const fn id(&self) -> &ProviderId {
        &self.id
    }

    /// Start one login: mint fresh state, nonce, and a PKCE verifier and return the provider redirect
    /// alongside the [`PendingLogin`] the caller must bind to the browser and echo back on callback.
    ///
    /// # Errors
    /// Returns [`OidcProviderError`] when discovery metadata cannot be fetched or validated.
    pub async fn authorization(&self, now: i64) -> Result<Authorization, OidcProviderError> {
        let endpoints = self.endpoints(now).await?;
        let verifier = random_token();
        let pending = PendingLogin {
            state: random_token(),
            nonce: random_token(),
            challenge: pkce_challenge(&verifier),
            verifier,
        };
        let mut redirect_url = endpoints.authorization;
        redirect_url
            .query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", self.redirect_uri.as_str())
            .append_pair("scope", &self.scope)
            .append_pair("state", &pending.state)
            .append_pair("nonce", &pending.nonce)
            .append_pair("code_challenge", &pending.challenge)
            .append_pair("code_challenge_method", "S256");
        Ok(Authorization { redirect_url, pending })
    }

    /// Finish one login: reject a callback whose state does not match, exchange the code, and validate
    /// the ID token against the pinned issuer, audience, nonce, and a cached signing key.
    ///
    /// # Errors
    /// Returns [`OidcProviderError`] on a state mismatch, a failed exchange, or an ID token that fails
    /// signature, issuer, audience, expiry, or nonce validation.
    pub async fn callback(
        &self,
        response: &CallbackResponse,
        pending: &PendingLogin,
        now: i64,
    ) -> Result<ExternalLogin, OidcProviderError> {
        if !crate::secrets_match(&response.state, &pending.state) {
            return Err(OidcProviderError::StateMismatch);
        }
        let endpoints = self.endpoints(now).await?;
        let id_token = self
            .exchange(&endpoints.token, &response.code, &pending.verifier)
            .await?;
        self.identity(&id_token, &pending.nonce, now).await
    }

    async fn exchange(&self, token_endpoint: &Url, code: &str, verifier: &str) -> Result<String, OidcProviderError> {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "authorization_code")
            .append_pair("code", code)
            .append_pair("redirect_uri", self.redirect_uri.as_str())
            .append_pair("code_verifier", verifier)
            .append_pair("client_id", &self.client_id)
            .finish();
        let mut request = self
            .client
            .post(token_endpoint.clone())
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body);
        if let Some(secret) = &self.client_secret {
            request = request.basic_auth(&self.client_id, Some(secret));
        }
        let (body, _) = self
            .fetch(request, TOKEN_BODY_LIMIT)
            .await
            .map_err(|error| match error {
                OidcProviderError::InvalidProviderResponse => OidcProviderError::TokenExchange,
                other => other,
            })?;
        serde_json::from_slice::<TokenResponse>(&body)
            .map(|response| response.id_token)
            .map_err(|_| OidcProviderError::TokenExchange)
    }

    async fn identity(&self, id_token: &str, nonce: &str, now: i64) -> Result<ExternalLogin, OidcProviderError> {
        let header = decode_header(id_token).map_err(|_| OidcProviderError::InvalidToken)?;
        if header.alg != Algorithm::RS256 {
            return Err(OidcProviderError::InvalidToken);
        }
        let kid = header
            .kid
            .filter(|kid| !kid.is_empty())
            .ok_or(OidcProviderError::InvalidToken)?;
        let key = self.key(&kid, now).await?;
        let mut validation = Validation::new(Algorithm::RS256);
        // jsonwebtoken validates `exp` against wall-clock time; `now` drives expiry here instead so the
        // caller owns one clock across cache freshness and token validity.
        validation.validate_exp = false;
        validation.validate_nbf = false;
        validation.set_required_spec_claims(&["iss", "aud", "sub"]);
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[&self.client_id]);
        let claims = decode::<IdTokenClaims>(id_token, &key, &validation)
            .map_err(|_| OidcProviderError::InvalidToken)?
            .claims;
        let skew = i64::try_from(self.leeway_secs).unwrap_or(i64::MAX);
        if claims.exp.saturating_add(skew) < now || claims.iat.saturating_sub(skew) > now {
            return Err(OidcProviderError::InvalidToken);
        }
        if !crate::secrets_match(&claims.nonce, nonce) {
            return Err(OidcProviderError::InvalidToken);
        }
        if claims.azp.as_ref().is_some_and(|azp| azp != &self.client_id) {
            return Err(OidcProviderError::InvalidToken);
        }
        self.login(&claims.extra)
    }

    fn login(&self, claims: &BTreeMap<String, Value>) -> Result<ExternalLogin, OidcProviderError> {
        let subject = claims
            .get(&self.subject_claim)
            .and_then(Value::as_str)
            .ok_or(OidcProviderError::InvalidClaims)?;
        let display = claims
            .get(&self.display_name_claim)
            .and_then(Value::as_str)
            .unwrap_or(subject);
        let mut groups = self
            .groups_claim
            .as_deref()
            .and_then(|claim| claims.get(claim))
            .map(claim_groups)
            .transpose()?
            .unwrap_or_default();
        groups.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        groups.dedup();
        ExternalLogin::new(
            ExternalIdentity::new(
                self.id.clone(),
                ExternalSubject::new(subject).map_err(|_| OidcProviderError::InvalidClaims)?,
            ),
            UserName::new(display).map_err(|_| OidcProviderError::InvalidClaims)?,
            groups,
        )
        .map_err(|_| OidcProviderError::InvalidClaims)
    }

    async fn endpoints(&self, now: i64) -> Result<Endpoints, OidcProviderError> {
        let cache = self.fresh(now, None).await?;
        cache.endpoints.clone().ok_or(OidcProviderError::Unavailable)
    }

    async fn key(&self, kid: &str, now: i64) -> Result<DecodingKey, OidcProviderError> {
        let cache = self.fresh(now, Some(kid)).await?;
        cache.keys.get(kid).cloned().ok_or(OidcProviderError::UnknownKey)
    }

    async fn fresh(
        &self,
        now: i64,
        want_key: Option<&str>,
    ) -> Result<tokio::sync::MutexGuard<'_, Cache>, OidcProviderError> {
        let mut cache = self.cache.lock().await;
        let stale = cache.endpoints.is_none() || now >= cache.fresh_until;
        let key_miss = want_key.is_some_and(|kid| !cache.keys.contains_key(kid));
        let hard_expired = cache.endpoints.is_some() && now >= cache.hard_until;
        if (stale || key_miss) && (now >= cache.refresh_after || hard_expired) {
            match self.refresh(now).await {
                Ok(next) => *cache = next,
                Err(error) => {
                    cache.refresh_after = now + MIN_FRESH_SECS;
                    if cache.endpoints.is_none() || hard_expired {
                        return Err(error);
                    }
                }
            }
        }
        Ok(cache)
    }

    async fn refresh(&self, now: i64) -> Result<Cache, OidcProviderError> {
        let (body, discovery_age) = self
            .fetch(self.client.get(self.discovery.clone()), DISCOVERY_BODY_LIMIT)
            .await?;
        let discovery =
            serde_json::from_slice::<Discovery>(&body).map_err(|_| OidcProviderError::InvalidProviderResponse)?;
        if discovery.issuer != self.issuer || !discovery.algorithms.iter().any(|algorithm| algorithm == "RS256") {
            return Err(OidcProviderError::InvalidProviderResponse);
        }
        let authorization = self.endpoint(&discovery.authorization_endpoint)?;
        let token = self.endpoint(&discovery.token_endpoint)?;
        let jwks_uri = self.endpoint(&discovery.jwks_uri)?;
        let (jwks_body, jwks_age) = self.fetch(self.client.get(jwks_uri), JWKS_BODY_LIMIT).await?;
        let jwks =
            serde_json::from_slice::<JwkSet>(&jwks_body).map_err(|_| OidcProviderError::InvalidProviderResponse)?;
        let keys = usable_keys(jwks)?;
        let fresh_for = discovery_age
            .unwrap_or(DEFAULT_FRESH_SECS)
            .min(jwks_age.unwrap_or(DEFAULT_FRESH_SECS))
            .clamp(MIN_FRESH_SECS, MAX_FRESH_SECS);
        Ok(Cache {
            endpoints: Some(Endpoints { authorization, token }),
            keys,
            fresh_until: now + fresh_for,
            hard_until: now + HARD_CACHE_SECS,
            refresh_after: now + MIN_FRESH_SECS,
        })
    }

    fn endpoint(&self, value: &str) -> Result<Url, OidcProviderError> {
        Url::parse(value)
            .ok()
            .filter(|url| secure_url(url, self.allow_insecure).is_some())
            .ok_or(OidcProviderError::InvalidProviderResponse)
    }

    async fn fetch(
        &self,
        request: reqwest::RequestBuilder,
        limit: usize,
    ) -> Result<(Vec<u8>, Option<i64>), OidcProviderError> {
        let mut response = request.send().await.map_err(|_| OidcProviderError::Unavailable)?;
        if !response.status().is_success()
            || response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_none_or(|value| !is_json_content_type(value))
            || response
                .headers()
                .get(CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<usize>().ok())
                .is_some_and(|length| length > limit)
        {
            return Err(OidcProviderError::InvalidProviderResponse);
        }
        let max_age = response
            .headers()
            .get(CACHE_CONTROL)
            .and_then(|value| value.to_str().ok())
            .and_then(cache_max_age);
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| OidcProviderError::Unavailable)? {
            if body.len() + chunk.len() > limit {
                return Err(OidcProviderError::InvalidProviderResponse);
            }
            body.extend_from_slice(&chunk);
        }
        Ok((body, max_age))
    }
}

/// Browser OIDC login followed by provider-neutral external identity linking.
#[derive(Clone)]
pub struct OidcLoginService<S> {
    provider: OidcLoginProvider,
    linker: ExternalIdentityLinker<S>,
    mappings: Arc<[ExternalGroupGrant]>,
}

impl<S> std::fmt::Debug for OidcLoginService<S> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OidcLoginService")
            .field("provider", &self.provider)
            .field("group_mappings", &self.mappings.len())
            .finish_non_exhaustive()
    }
}

impl<S: ExternalIdentityStore + Sync> OidcLoginService<S> {
    #[must_use]
    pub fn new(provider: OidcLoginProvider, store: S, mappings: Vec<ExternalGroupGrant>) -> Self {
        Self {
            provider,
            linker: ExternalIdentityLinker::new(store),
            mappings: mappings.into(),
        }
    }

    #[must_use]
    pub const fn id(&self) -> &ProviderId {
        self.provider.id()
    }

    /// Start one browser login and return the provider redirect and its [`PendingLogin`].
    ///
    /// # Errors
    /// Returns [`OidcProviderError`] when discovery metadata cannot be fetched or validated.
    pub async fn authorization(&self, now: i64) -> Result<Authorization, OidcProviderError> {
        self.provider.authorization(now).await
    }

    /// Finish one browser login and commit its link and managed grants after the ID token validates.
    ///
    /// # Errors
    /// Returns [`OidcLoginError::Provider`] for a rejected or failed exchange and [`OidcLoginError::Store`]
    /// when the atomic local link cannot commit.
    pub async fn callback(
        &self,
        response: &CallbackResponse,
        pending: &PendingLogin,
        now: i64,
    ) -> Result<ExternalIdentityResolution, OidcLoginError<S::Error>> {
        let login = self
            .provider
            .callback(response, pending, now)
            .await
            .map_err(OidcLoginError::Provider)?;
        self.linker
            .link_or_resolve(&login, &self.mappings)
            .map_err(OidcLoginError::Store)
    }
}

/// The provider redirect and the transient secrets one login attempt is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authorization {
    pub redirect_url: Url,
    pub pending: PendingLogin,
}

/// The per-attempt state a caller stores against the browser and echoes back on callback. Its fields
/// are secrets: bind it to the browser session and treat the state as single-use.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingLogin {
    pub state: String,
    pub nonce: String,
    pub verifier: String,
    pub challenge: String,
}

impl std::fmt::Debug for PendingLogin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PendingLogin([redacted])")
    }
}

/// The authorization response a provider returns to the callback URL.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CallbackResponse {
    pub state: String,
    pub code: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OidcProviderBuildError {
    #[error("OIDC issuer must be an https URL without query or fragment")]
    InvalidIssuer,
    #[error("OIDC redirect URI must be an https URL without a fragment")]
    InvalidRedirectUri,
    #[error("OIDC client ID must not be empty")]
    EmptyClientId,
    #[error("OIDC subject, display-name, and group claim names must not be empty")]
    InvalidClaim,
    #[error("OIDC request timeout must be positive")]
    InvalidTimeout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum OidcProviderError {
    #[error("OIDC provider unavailable")]
    Unavailable,
    #[error("OIDC provider returned an invalid response")]
    InvalidProviderResponse,
    #[error("OIDC provider names an unknown signing key")]
    UnknownKey,
    #[error("OIDC callback state does not match the pending login")]
    StateMismatch,
    #[error("OIDC authorization code exchange failed")]
    TokenExchange,
    #[error("OIDC ID token failed validation")]
    InvalidToken,
    #[error("OIDC ID token claims do not match the configured mapping")]
    InvalidClaims,
}

impl OidcProviderError {
    /// Whether the failure is a provider-side outage a caller should surface as unavailable rather than
    /// as a rejected login.
    #[must_use]
    pub const fn unavailable(&self) -> bool {
        matches!(
            self,
            Self::Unavailable | Self::InvalidProviderResponse | Self::UnknownKey
        )
    }
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum OidcLoginError<E> {
    #[error("OIDC provider failed: {0}")]
    Provider(OidcProviderError),
    #[error("external identity store failed: {0}")]
    Store(E),
}

#[derive(Default)]
struct Cache {
    endpoints: Option<Endpoints>,
    keys: HashMap<String, DecodingKey>,
    fresh_until: i64,
    hard_until: i64,
    refresh_after: i64,
}

#[derive(Clone)]
struct Endpoints {
    authorization: Url,
    token: Url,
}

#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    #[serde(default, rename = "id_token_signing_alg_values_supported")]
    algorithms: Vec<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    id_token: String,
}

#[derive(Deserialize)]
struct IdTokenClaims {
    exp: i64,
    iat: i64,
    nonce: String,
    #[serde(default)]
    azp: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

fn random_token() -> String {
    let mut bytes = [0u8; RANDOM_BYTES];
    getrandom::fill(&mut bytes).expect("operating system RNG is available");
    URL_SAFE_NO_PAD.encode(bytes)
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn scope_string(scopes: &[String]) -> String {
    let mut ordered = Vec::with_capacity(scopes.len() + 1);
    ordered.push(OPENID_SCOPE);
    for scope in scopes {
        if scope != OPENID_SCOPE && !ordered.contains(&scope.as_str()) {
            ordered.push(scope);
        }
    }
    ordered.join(" ")
}

fn claim_groups(value: &Value) -> Result<Vec<ExternalGroup>, OidcProviderError> {
    let names = match value {
        Value::String(single) => vec![single.as_str()],
        Value::Array(values) => values.iter().filter_map(Value::as_str).collect(),
        _ => return Err(OidcProviderError::InvalidClaims),
    };
    names
        .into_iter()
        .map(|name| ExternalGroup::new(name).map_err(|_| OidcProviderError::InvalidClaims))
        .collect()
}

fn secure_url(url: &Url, allow_insecure: bool) -> Option<&Url> {
    let scheme_ok = if allow_insecure {
        matches!(url.scheme(), "http" | "https")
    } else {
        url.scheme() == "https"
    };
    (scheme_ok && url.host_str().is_some() && url.username().is_empty() && url.password().is_none()).then_some(url)
}

fn discovery_url(issuer: &Url) -> Url {
    let mut discovery = issuer.clone();
    let path = format!(
        "{}/.well-known/openid-configuration",
        issuer.path().trim_end_matches('/')
    );
    discovery.set_path(&path);
    discovery
}

fn usable_keys(jwks: JwkSet) -> Result<HashMap<String, DecodingKey>, OidcProviderError> {
    let mut ids = HashSet::new();
    if jwks.keys.is_empty()
        || jwks.keys.iter().any(|key| {
            let Some(id) = key.common.key_id.as_deref().filter(|id| !id.is_empty()) else {
                return true;
            };
            !ids.insert(id)
        })
    {
        return Err(OidcProviderError::InvalidProviderResponse);
    }
    let mut keys = HashMap::new();
    for key in jwks.keys.into_iter().filter(|key| {
        matches!(key.algorithm, AlgorithmParameters::RSA(_))
            && key
                .common
                .key_algorithm
                .is_none_or(|algorithm| algorithm.to_string() == "RS256")
            && key
                .common
                .public_key_use
                .as_ref()
                .is_none_or(|usage| usage == &PublicKeyUse::Signature)
            && key
                .common
                .key_operations
                .as_ref()
                .is_none_or(|operations| operations.contains(&KeyOperations::Verify))
    }) {
        let id = key
            .common
            .key_id
            .clone()
            .expect("JWKS key ID validation precedes decoding");
        let decoding = DecodingKey::from_jwk(&key).map_err(|_| OidcProviderError::InvalidProviderResponse)?;
        keys.insert(id, decoding);
    }
    if keys.is_empty() {
        return Err(OidcProviderError::InvalidProviderResponse);
    }
    Ok(keys)
}

fn is_json_content_type(value: &str) -> bool {
    let media = value
        .split(';')
        .next()
        .map(str::trim)
        .unwrap_or_default()
        .to_ascii_lowercase();
    media == "application/json" || media.starts_with("application/") && media.ends_with("+json")
}

fn cache_max_age(value: &str) -> Option<i64> {
    value.split(',').find_map(|directive| {
        let (name, value) = directive.trim().split_once('=')?;
        name.eq_ignore_ascii_case("max-age")
            .then(|| value.trim_matches('"').parse().ok())
            .flatten()
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use jsonwebtoken::{EncodingKey, Header};
    use rstest::rstest;
    use serde_json::{Value, json};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::{ExternalIdentityResolution, ExternalLinkRequest, ServerUser, UserId, UserState};

    const NOW: i64 = 2_000_000_000;
    const MODULUS: &str = "yRE6rHuNR0QbHO3H3Kt2pOKGVhQqGZXInOduQNxXzuKlvQTLUTv4l4sggh5_CYYi_cvI-SXVT9kPWSKXxJXBXd_4LkvcPuUakBoAkfh-eiFVMh2VrUyWyj3MFl0HTVF9KwRXLAcwkREiS3npThHRyIxuy0ZMeZfxVL5arMhw1SRELB8HoGfG_AtH89BIE9jDBHZ9dLelK9a184zAf8LwoPLxvJb3Il5nncqPcSfKDDodMFBIMc4lQzDKL5gvmiXLXB1AGLm8KBjfE8s3L5xqi-yUod-j8MtvIj812dkS4QMiRVN_by2h3ZY8LYVGrqZXZTcgn2ujn8uKjXLZVD5TdQ";
    const PRIVATE_KEY_DER: &str = "MIIEpAIBAAKCAQEAyRE6rHuNR0QbHO3H3Kt2pOKGVhQqGZXInOduQNxXzuKlvQTLUTv4l4sggh5/CYYi/cvI+SXVT9kPWSKXxJXBXd/4LkvcPuUakBoAkfh+eiFVMh2VrUyWyj3MFl0HTVF9KwRXLAcwkREiS3npThHRyIxuy0ZMeZfxVL5arMhw1SRELB8HoGfG/AtH89BIE9jDBHZ9dLelK9a184zAf8LwoPLxvJb3Il5nncqPcSfKDDodMFBIMc4lQzDKL5gvmiXLXB1AGLm8KBjfE8s3L5xqi+yUod+j8MtvIj812dkS4QMiRVN/by2h3ZY8LYVGrqZXZTcgn2ujn8uKjXLZVD5TdQIDAQABAoIBAHREk0I0O9DvECKdWUpAmF3mY7oY9PNQiu44Yaf+AoSuyRpRUGTMIgc3u3eivOE8ALX0BmYUO5JtuRNZDpvt4SAwqCnVUinIf6C+eH/wSurCpapSM0BAHp4aOA7igptyOMgMPYBHNA1e9A7jE0dCxKWMl3DSWNyjQTk4zeRGEAEfbNjHrq6YCtjHSZSLmWiG80hnfnYos9hOr5JnLnyS7ZmFE/5P3XVrxLc/tQ5zum0R4cbrgzHiQP5RgfxGJaEi7XcgherCCOgurJSSbYH29Gz8u5fFbS+Yg8s+OiCss3cs1rSgJ9/eHZuzGEdUZVARH6hVMjSuwvqVTFaE8AgtleECgYEA+uLMn4kNqHlJS2A5uAnCkj90ZxEtNm3E8hAxUrhssktY5XSOAPBlxyf5RuRGIImGtUVIr4HuJSa5TX48n3Vdt9MYCprO/iYl6moNRSPt5qowIIOJmIjY2mqPDfDt/zw+fcDD3lmCJrFlzcnh0uea1CohxEbQnL3cypeLt+WbU6kCgYEAzSp19m1ajieFkqgoB0YTpt/OroDx38vvI5unInJlEeOjQ+oIAQdN2wpxBvTrRorMU6P07mFUbt1j+Co6CbNiw+X8HcCaqYLR5clbJOOWNR36PuzOpQLkfK8woupBxzW9B8gZmY8rB1mbJ+/WTPrEJy6YGmIEBkWylQ2VpW8O4O0CgYEApdbvvfFBlwD9YxbrcGz7MeNCFbMz+MucqQntIKoKJ91ImPxvtc0y6e/Rhnv0oyNlaUOwJVu0yNgNG117w0g4t/+Q38mvVC5xV7/cn7x9UMFk6MkqVir3dYGEqIl/OP1grY2Tq9HtB5iyG9L8NIamQOLMyUqqMUILxdthHyFmiGkCgYEAn9+PjpjGMPHxL0gj8Q8VbzsFtou6b1deIRRA2CHmSltltR1gYVTMwXxQeUhPMmgkMqUXzs4/WijgpthY44hK1TaZEKIuoxrS70nJ4WQLf5a9k1065fDsFZD6yGjdGxvwEmlGMZgTwqV7t1I4X0Ilqhav5hcs5apYL7gnPYPeRz0CgYALHCj/Ji8XSsDoF/MhVhnGdIs2P99NNdmo3R2Pv0CuZbDKMU559LJHUvrKS8WkuWRDuKrz1W/EQKApFjDGpdqToZqriUFQzwy7mR3ayIiogzNtHcvbDHx8oFnGY0OFksX/ye0/XGpy2SFxYRwGU98HPYeBvAQQrVjdkzfy7BmXQQ==";

    fn settings(issuer: &str) -> OidcProviderSettings {
        OidcProviderSettings {
            id: ProviderId::new("corporate").unwrap(),
            issuer: Url::parse(issuer).unwrap(),
            client_id: "peryx-web".to_owned(),
            client_secret: Some("s3cret".to_owned()),
            redirect_uri: Url::parse("https://registry.example/oidc/corporate/callback").unwrap(),
            scopes: vec!["email".to_owned(), "groups".to_owned()],
            subject_claim: "sub".to_owned(),
            display_name_claim: "name".to_owned(),
            groups_claim: Some("groups".to_owned()),
            clock_skew: Duration::from_mins(1),
            request_timeout: Duration::from_secs(5),
        }
    }

    fn provider(issuer: &str) -> OidcLoginProvider {
        OidcLoginProvider::build(settings(issuer), true).unwrap()
    }

    fn encoding_key() -> EncodingKey {
        use base64::engine::general_purpose::STANDARD;
        EncodingKey::from_rsa_der(&STANDARD.decode(PRIVATE_KEY_DER).unwrap())
    }

    fn jwk(kid: &str) -> Value {
        json!({"kty": "RSA", "n": MODULUS, "e": "AQAB", "kid": kid, "alg": "RS256", "use": "sig"})
    }

    fn mint(kid: &str, claims: &Value) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.to_owned());
        jsonwebtoken::encode(&header, claims, &encoding_key()).unwrap()
    }

    fn issuer(server: &MockServer) -> String {
        format!("{}/", server.uri())
    }

    fn base_claims(server: &MockServer) -> Value {
        json!({
            "iss": issuer(server),
            "aud": "peryx-web",
            "exp": NOW + 7200,
            "iat": NOW,
            "nonce": "nonce-abc",
            "sub": "subject-123",
            "name": "Ada Lovelace",
            "groups": ["dev", "ops"],
        })
    }

    fn pending() -> PendingLogin {
        PendingLogin {
            state: "state-abc".to_owned(),
            nonce: "nonce-abc".to_owned(),
            verifier: "verifier-abc".to_owned(),
            challenge: "challenge-abc".to_owned(),
        }
    }

    fn response() -> CallbackResponse {
        CallbackResponse {
            state: "state-abc".to_owned(),
            code: "auth-code".to_owned(),
        }
    }

    async fn mount_metadata(server: &MockServer, keys: Value) {
        mount_discovery(
            server,
            json!({
                "issuer": issuer(server),
                "authorization_endpoint": format!("{}/authorize", server.uri()),
                "token_endpoint": format!("{}/token", server.uri()),
                "jwks_uri": format!("{}/jwks", server.uri()),
                "id_token_signing_alg_values_supported": ["RS256"],
            }),
            "application/json",
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/jwks"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_json(keys),
            )
            .mount(server)
            .await;
    }

    async fn mount_discovery(server: &MockServer, body: Value, content_type: &str) {
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("cache-control", "max-age=120")
                    .set_body_raw(body.to_string(), content_type),
            )
            .mount(server)
            .await;
    }

    async fn mount_token(server: &MockServer, body: Value) {
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_json(body),
            )
            .mount(server)
            .await;
    }

    async fn ready() -> (MockServer, OidcLoginProvider) {
        let server = MockServer::start().await;
        mount_metadata(&server, json!({"keys": [jwk("key-1")]})).await;
        let provider = provider(&server.uri());
        (server, provider)
    }

    #[tokio::test]
    async fn test_authorization_binds_state_nonce_and_pkce() {
        let (_server, provider) = ready().await;
        let authorization = provider.authorization(NOW).await.unwrap();
        let query: std::collections::HashMap<_, _> = authorization.redirect_url.query_pairs().into_owned().collect();
        assert_eq!(query["response_type"], "code");
        assert_eq!(query["client_id"], "peryx-web");
        assert_eq!(
            query["redirect_uri"],
            "https://registry.example/oidc/corporate/callback"
        );
        assert_eq!(query["code_challenge_method"], "S256");
        assert_eq!(query["state"], authorization.pending.state);
        assert_eq!(query["nonce"], authorization.pending.nonce);
        assert_eq!(query["code_challenge"], pkce_challenge(&authorization.pending.verifier));
        assert_eq!(query["code_challenge"], authorization.pending.challenge);
        assert!(query["scope"].split(' ').eq(["openid", "email", "groups"]));
        assert_ne!(authorization.pending.state, authorization.pending.nonce);
    }

    #[tokio::test]
    async fn test_valid_response_yields_the_linked_subject() {
        let (server, provider) = ready().await;
        let authorization = provider.authorization(NOW).await.unwrap();
        let mut claims = base_claims(&server);
        claims["nonce"] = authorization.pending.nonce.clone().into();
        mount_token(
            &server,
            json!({"id_token": mint("key-1", &claims), "token_type": "Bearer"}),
        )
        .await;
        let callback = CallbackResponse {
            state: authorization.pending.state.clone(),
            code: "auth-code".to_owned(),
        };
        let login = provider.callback(&callback, &authorization.pending, NOW).await.unwrap();
        assert_eq!(login.identity.subject.as_str(), "subject-123");
        assert_eq!(login.identity.provider.as_str(), "corporate");
        assert_eq!(login.display_name.display(), "Ada Lovelace");
        assert_eq!(
            login.groups.iter().map(ExternalGroup::as_str).collect::<Vec<_>>(),
            vec!["dev", "ops"]
        );
    }

    #[tokio::test]
    async fn test_callback_rejects_a_mismatched_state() {
        let provider = provider("https://issuer.example");
        let callback = CallbackResponse {
            state: "forged".to_owned(),
            code: "auth-code".to_owned(),
        };
        assert!(matches!(
            provider.callback(&callback, &pending(), NOW).await,
            Err(OidcProviderError::StateMismatch)
        ));
    }

    #[rstest]
    #[case::wrong_issuer(json!("https://evil.example"), "iss")]
    #[case::wrong_audience(json!("other-client"), "aud")]
    #[case::expired(json!(NOW - 3600), "exp")]
    #[case::future_issued(json!(NOW + 7200), "iat")]
    #[tokio::test]
    async fn test_id_token_registered_claims_are_enforced(#[case] value: Value, #[case] claim: &str) {
        let server = MockServer::start().await;
        mount_metadata(&server, json!({"keys": [jwk("key-1")]})).await;
        let provider = provider(&server.uri());
        let mut claims = base_claims(&server);
        claims[claim] = value;
        mount_token(
            &server,
            json!({"id_token": mint("key-1", &claims), "token_type": "Bearer"}),
        )
        .await;
        assert!(matches!(
            provider.callback(&response(), &pending(), NOW).await,
            Err(OidcProviderError::InvalidToken)
        ));
    }

    #[tokio::test]
    async fn test_nonce_mismatch_fails_the_login() {
        let (server, provider) = ready().await;
        let mut claims = base_claims(&server);
        claims["nonce"] = "different".into();
        mount_token(
            &server,
            json!({"id_token": mint("key-1", &claims), "token_type": "Bearer"}),
        )
        .await;
        assert!(matches!(
            provider.callback(&response(), &pending(), NOW).await,
            Err(OidcProviderError::InvalidToken)
        ));
    }

    #[tokio::test]
    async fn test_bad_signature_fails_the_login() {
        let (server, provider) = ready().await;
        let mut token = mint("key-1", &base_claims(&server));
        token.push('x');
        mount_token(&server, json!({"id_token": token, "token_type": "Bearer"})).await;
        assert!(matches!(
            provider.callback(&response(), &pending(), NOW).await,
            Err(OidcProviderError::InvalidToken)
        ));
    }

    #[tokio::test]
    async fn test_non_rs256_algorithm_is_rejected() {
        let (server, provider) = ready().await;
        let token = jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &base_claims(&server),
            &EncodingKey::from_secret(b"secret"),
        )
        .unwrap();
        mount_token(&server, json!({"id_token": token, "token_type": "Bearer"})).await;
        assert!(matches!(
            provider.callback(&response(), &pending(), NOW).await,
            Err(OidcProviderError::InvalidToken)
        ));
    }

    #[tokio::test]
    async fn test_missing_key_id_is_rejected() {
        let (server, provider) = ready().await;
        let token =
            jsonwebtoken::encode(&Header::new(Algorithm::RS256), &base_claims(&server), &encoding_key()).unwrap();
        mount_token(&server, json!({"id_token": token, "token_type": "Bearer"})).await;
        assert!(matches!(
            provider.callback(&response(), &pending(), NOW).await,
            Err(OidcProviderError::InvalidToken)
        ));
    }

    #[tokio::test]
    async fn test_authorized_party_must_match_the_client() {
        let (server, provider) = ready().await;
        let mut claims = base_claims(&server);
        claims["azp"] = "someone-else".into();
        mount_token(
            &server,
            json!({"id_token": mint("key-1", &claims), "token_type": "Bearer"}),
        )
        .await;
        assert!(matches!(
            provider.callback(&response(), &pending(), NOW).await,
            Err(OidcProviderError::InvalidToken)
        ));
    }

    #[tokio::test]
    async fn test_matching_authorized_party_is_accepted() {
        let (server, provider) = ready().await;
        let mut claims = base_claims(&server);
        claims["azp"] = "peryx-web".into();
        mount_token(
            &server,
            json!({"id_token": mint("key-1", &claims), "token_type": "Bearer"}),
        )
        .await;
        assert!(provider.callback(&response(), &pending(), NOW).await.is_ok());
    }

    #[tokio::test]
    async fn test_missing_subject_claim_is_rejected() {
        let server = MockServer::start().await;
        mount_metadata(&server, json!({"keys": [jwk("key-1")]})).await;
        let mut raw = settings(&server.uri());
        raw.subject_claim = "employee_id".to_owned();
        let provider = OidcLoginProvider::build(raw, true).unwrap();
        mount_token(
            &server,
            json!({"id_token": mint("key-1", &base_claims(&server)), "token_type": "Bearer"}),
        )
        .await;
        assert!(matches!(
            provider.callback(&response(), &pending(), NOW).await,
            Err(OidcProviderError::InvalidClaims)
        ));
    }

    #[tokio::test]
    async fn test_absent_display_name_falls_back_to_the_subject() {
        let (server, provider) = ready().await;
        let mut claims = base_claims(&server);
        claims.as_object_mut().unwrap().remove("name");
        mount_token(
            &server,
            json!({"id_token": mint("key-1", &claims), "token_type": "Bearer"}),
        )
        .await;
        let login = provider.callback(&response(), &pending(), NOW).await.unwrap();
        assert_eq!(login.display_name.display(), "subject-123");
    }

    #[tokio::test]
    async fn test_blank_display_name_is_rejected() {
        let (server, provider) = ready().await;
        let mut claims = base_claims(&server);
        claims["name"] = "   ".into();
        mount_token(
            &server,
            json!({"id_token": mint("key-1", &claims), "token_type": "Bearer"}),
        )
        .await;
        assert!(matches!(
            provider.callback(&response(), &pending(), NOW).await,
            Err(OidcProviderError::InvalidClaims)
        ));
    }

    #[tokio::test]
    async fn test_single_string_group_is_accepted() {
        let (server, provider) = ready().await;
        let mut claims = base_claims(&server);
        claims["groups"] = "solo".into();
        mount_token(
            &server,
            json!({"id_token": mint("key-1", &claims), "token_type": "Bearer"}),
        )
        .await;
        let login = provider.callback(&response(), &pending(), NOW).await.unwrap();
        assert_eq!(
            login.groups.iter().map(ExternalGroup::as_str).collect::<Vec<_>>(),
            vec!["solo"]
        );
    }

    #[tokio::test]
    async fn test_absent_group_claim_asserts_no_groups() {
        let server = MockServer::start().await;
        mount_metadata(&server, json!({"keys": [jwk("key-1")]})).await;
        let mut raw = settings(&server.uri());
        raw.groups_claim = None;
        let provider = OidcLoginProvider::build(raw, true).unwrap();
        mount_token(
            &server,
            json!({"id_token": mint("key-1", &base_claims(&server)), "token_type": "Bearer"}),
        )
        .await;
        let login = provider.callback(&response(), &pending(), NOW).await.unwrap();
        assert!(login.groups.is_empty());
    }

    #[rstest]
    #[case::wrong_type(json!(5))]
    #[case::control_character(json!(["ok", "b\u{0001}d"]))]
    #[tokio::test]
    async fn test_invalid_group_claim_is_rejected(#[case] groups: Value) {
        let server = MockServer::start().await;
        mount_metadata(&server, json!({"keys": [jwk("key-1")]})).await;
        let provider = provider(&server.uri());
        let mut claims = base_claims(&server);
        claims["groups"] = groups;
        mount_token(
            &server,
            json!({"id_token": mint("key-1", &claims), "token_type": "Bearer"}),
        )
        .await;
        assert!(matches!(
            provider.callback(&response(), &pending(), NOW).await,
            Err(OidcProviderError::InvalidClaims)
        ));
    }

    #[tokio::test]
    async fn test_token_exchange_error_status_fails_closed() {
        let (server, provider) = ready().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(400))
            .mount(&server)
            .await;
        assert!(matches!(
            provider.callback(&response(), &pending(), NOW).await,
            Err(OidcProviderError::TokenExchange)
        ));
    }

    #[tokio::test]
    async fn test_token_response_without_an_id_token_is_rejected() {
        let (server, provider) = ready().await;
        mount_token(&server, json!({"token_type": "Bearer"})).await;
        assert!(matches!(
            provider.callback(&response(), &pending(), NOW).await,
            Err(OidcProviderError::TokenExchange)
        ));
    }

    #[tokio::test]
    async fn test_token_exchange_transport_failure_stays_unavailable() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead = listener.local_addr().unwrap();
        drop(listener);
        let server = MockServer::start().await;
        mount_discovery(
            &server,
            json!({
                "issuer": issuer(&server),
                "authorization_endpoint": format!("{}/authorize", server.uri()),
                "token_endpoint": format!("http://{dead}/token"),
                "jwks_uri": format!("{}/jwks", server.uri()),
                "id_token_signing_alg_values_supported": ["RS256"],
            }),
            "application/json",
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/jwks"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_json(json!({"keys": [jwk("key-1")]})),
            )
            .mount(&server)
            .await;
        assert!(matches!(
            provider(&server.uri()).callback(&response(), &pending(), NOW).await,
            Err(OidcProviderError::Unavailable)
        ));
    }

    #[tokio::test]
    async fn test_discovery_pins_the_issuer() {
        let server = MockServer::start().await;
        mount_discovery(
            &server,
            json!({
                "issuer": "https://other.example",
                "authorization_endpoint": format!("{}/authorize", server.uri()),
                "token_endpoint": format!("{}/token", server.uri()),
                "jwks_uri": format!("{}/jwks", server.uri()),
                "id_token_signing_alg_values_supported": ["RS256"],
            }),
            "application/json",
        )
        .await;
        let provider = provider(&server.uri());
        assert!(matches!(
            provider.authorization(NOW).await,
            Err(OidcProviderError::InvalidProviderResponse)
        ));
    }

    #[tokio::test]
    async fn test_discovery_requires_rs256_support() {
        let server = MockServer::start().await;
        mount_discovery(
            &server,
            json!({
                "issuer": issuer(&server),
                "authorization_endpoint": format!("{}/authorize", server.uri()),
                "token_endpoint": format!("{}/token", server.uri()),
                "jwks_uri": format!("{}/jwks", server.uri()),
                "id_token_signing_alg_values_supported": ["ES256"],
            }),
            "application/json",
        )
        .await;
        assert!(matches!(
            provider(&server.uri()).authorization(NOW).await,
            Err(OidcProviderError::InvalidProviderResponse)
        ));
    }

    #[tokio::test]
    async fn test_invalid_endpoint_url_is_rejected() {
        let server = MockServer::start().await;
        mount_discovery(
            &server,
            json!({
                "issuer": issuer(&server),
                "authorization_endpoint": "ftp://insecure.example/authorize",
                "token_endpoint": format!("{}/token", server.uri()),
                "jwks_uri": format!("{}/jwks", server.uri()),
                "id_token_signing_alg_values_supported": ["RS256"],
            }),
            "application/json",
        )
        .await;
        assert!(matches!(
            provider(&server.uri()).authorization(NOW).await,
            Err(OidcProviderError::InvalidProviderResponse)
        ));
    }

    #[tokio::test]
    async fn test_non_json_discovery_is_rejected() {
        let server = MockServer::start().await;
        mount_discovery(&server, json!({"issuer": server.uri()}), "text/plain").await;
        assert!(matches!(
            provider(&server.uri()).authorization(NOW).await,
            Err(OidcProviderError::InvalidProviderResponse)
        ));
    }

    #[tokio::test]
    async fn test_oversize_discovery_is_rejected() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw("x".repeat(DISCOVERY_BODY_LIMIT + 1), "application/json"),
            )
            .mount(&server)
            .await;
        assert!(matches!(
            provider(&server.uri()).authorization(NOW).await,
            Err(OidcProviderError::InvalidProviderResponse)
        ));
    }

    #[tokio::test]
    async fn test_cold_provider_failure_is_rate_limited() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let provider = provider(&server.uri());
        assert!(matches!(provider.authorization(NOW).await, Err(error) if error.unavailable()));
        assert!(matches!(provider.authorization(NOW).await, Err(error) if error.unavailable()));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_cold_provider_network_failure_is_unavailable() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        assert!(matches!(
            provider(&format!("http://{address}")).authorization(NOW).await,
            Err(OidcProviderError::Unavailable)
        ));
    }

    #[tokio::test]
    async fn test_signing_key_rotation_succeeds_after_refresh() {
        let (server, provider) = ready().await;
        mount_token(
            &server,
            json!({"id_token": mint("key-1", &base_claims(&server)), "token_type": "Bearer"}),
        )
        .await;
        provider.callback(&response(), &pending(), NOW).await.unwrap();
        server.reset().await;
        mount_metadata(&server, json!({"keys": [jwk("key-2")]})).await;
        mount_token(
            &server,
            json!({"id_token": mint("key-2", &base_claims(&server)), "token_type": "Bearer"}),
        )
        .await;
        assert!(
            provider
                .callback(&response(), &pending(), NOW + MAX_FRESH_SECS)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn test_unknown_key_refresh_is_rate_limited() {
        let (server, provider) = ready().await;
        mount_token(
            &server,
            json!({"id_token": mint("key-1", &base_claims(&server)), "token_type": "Bearer"}),
        )
        .await;
        provider.callback(&response(), &pending(), NOW).await.unwrap();
        server.reset().await;
        mount_metadata(&server, json!({"keys": [jwk("key-1")]})).await;
        mount_token(
            &server,
            json!({"id_token": mint("key-2", &base_claims(&server)), "token_type": "Bearer"}),
        )
        .await;
        assert!(matches!(
            provider.callback(&response(), &pending(), NOW).await,
            Err(OidcProviderError::UnknownKey)
        ));
        assert!(matches!(
            provider.callback(&response(), &pending(), NOW + MIN_FRESH_SECS).await,
            Err(OidcProviderError::UnknownKey)
        ));
    }

    #[tokio::test]
    async fn test_metadata_outage_keeps_the_cached_key_then_hard_expires() {
        let (server, provider) = ready().await;
        mount_token(
            &server,
            json!({"id_token": mint("key-1", &base_claims(&server)), "token_type": "Bearer"}),
        )
        .await;
        provider.callback(&response(), &pending(), NOW).await.unwrap();
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        mount_token(
            &server,
            json!({"id_token": mint("key-1", &base_claims(&server)), "token_type": "Bearer"}),
        )
        .await;
        assert!(
            provider
                .callback(&response(), &pending(), NOW + MAX_FRESH_SECS)
                .await
                .is_ok()
        );
        assert!(matches!(
            provider.callback(&response(), &pending(), NOW + HARD_CACHE_SECS + 1).await,
            Err(error) if error.unavailable()
        ));
    }

    #[tokio::test]
    async fn test_warm_cache_serves_without_refetching() {
        let (server, provider) = ready().await;
        mount_token(
            &server,
            json!({"id_token": mint("key-1", &base_claims(&server)), "token_type": "Bearer"}),
        )
        .await;
        provider.callback(&response(), &pending(), NOW).await.unwrap();
        provider.callback(&response(), &pending(), NOW + 1).await.unwrap();
        let discovery = server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|request| request.url.path() == "/.well-known/openid-configuration")
            .count();
        assert_eq!(discovery, 1);
    }

    #[rstest]
    #[case::empty(json!({"keys": []}))]
    #[case::duplicate(json!({"keys": [jwk("dup"), jwk("dup")]}))]
    #[case::no_kid(json!({"keys": [{"kty": "RSA", "n": MODULUS, "e": "AQAB", "alg": "RS256"}]}))]
    #[case::no_usable(json!({"keys": [{"kty": "oct", "k": "c2VjcmV0", "kid": "sym", "alg": "HS256"}]}))]
    #[case::bad_modulus(json!({"keys": [{"kty": "RSA", "n": "!", "e": "AQAB", "kid": "key-1", "alg": "RS256"}]}))]
    #[tokio::test]
    async fn test_unusable_key_sets_are_rejected(#[case] keys: Value) {
        let server = MockServer::start().await;
        mount_metadata(&server, keys).await;
        assert!(matches!(
            provider(&server.uri()).authorization(NOW).await,
            Err(OidcProviderError::InvalidProviderResponse)
        ));
    }

    #[tokio::test]
    async fn test_verify_only_key_survives_alongside_incompatible_keys() {
        let server = MockServer::start().await;
        mount_metadata(
            &server,
            json!({"keys": [
                {"kty": "oct", "k": "c2VjcmV0", "kid": "sym", "alg": "HS256"},
                {"kty": "RSA", "n": MODULUS, "e": "AQAB", "kid": "sign-only", "alg": "RS256", "use": "sig", "key_ops": ["sign"]},
                {"kty": "RSA", "n": MODULUS, "e": "AQAB", "kid": "key-1", "alg": "RS256", "use": "sig", "key_ops": ["verify"]}
            ]}),
        )
        .await;
        let provider = provider(&server.uri());
        mount_token(
            &server,
            json!({"id_token": mint("key-1", &base_claims(&server)), "token_type": "Bearer"}),
        )
        .await;
        assert!(provider.callback(&response(), &pending(), NOW).await.is_ok());
    }

    #[tokio::test]
    async fn test_chunked_discovery_body_is_bounded_while_streaming() {
        use std::io::{Read as _, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0; 1024];
            let _ = socket.read(&mut request);
            let body = "x".repeat(DISCOVERY_BODY_LIMIT + 1);
            write!(
                socket,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n{:X}\r\n{body}\r\n0\r\n\r\n",
                body.len()
            )
            .unwrap();
        });
        assert!(matches!(
            provider(&format!("http://{address}")).authorization(NOW).await,
            Err(OidcProviderError::InvalidProviderResponse)
        ));
    }

    #[tokio::test]
    async fn test_truncated_discovery_body_is_unavailable() {
        use std::io::{Read as _, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0; 1024];
            let _ = socket.read(&mut request);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 8\r\nconnection: close\r\n\r\n{}")
                .unwrap();
        });
        assert!(matches!(
            provider(&format!("http://{address}")).authorization(NOW).await,
            Err(OidcProviderError::Unavailable)
        ));
    }

    #[rstest]
    #[case::json("application/json; charset=utf-8", true)]
    #[case::structured("application/jwk-set+json", true)]
    #[case::wrong("text/json", false)]
    fn test_json_content_type_classification(#[case] value: &str, #[case] accepted: bool) {
        assert_eq!(is_json_content_type(value), accepted);
    }

    #[rstest]
    #[case::present("max-age=42", Some(42))]
    #[case::quoted("private, max-age=\"7\"", Some(7))]
    #[case::absent("no-store", None)]
    fn test_cache_max_age_parsing(#[case] value: &str, #[case] expected: Option<i64>) {
        assert_eq!(cache_max_age(value), expected);
    }

    #[test]
    fn test_scope_string_inserts_and_deduplicates_openid() {
        assert_eq!(scope_string(&[]), "openid");
        assert_eq!(
            scope_string(&["openid".to_owned(), "email".to_owned(), "email".to_owned()]),
            "openid email"
        );
    }

    #[rstest]
    #[case::http_issuer(OidcProviderBuildError::InvalidIssuer, |s: &mut OidcProviderSettings| s.issuer = Url::parse("http://issuer.example").unwrap())]
    #[case::issuer_query(OidcProviderBuildError::InvalidIssuer, |s: &mut OidcProviderSettings| s.issuer = Url::parse("https://issuer.example/?x=1").unwrap())]
    #[case::redirect_fragment(OidcProviderBuildError::InvalidRedirectUri, |s: &mut OidcProviderSettings| s.redirect_uri = Url::parse("https://app.example/cb#x").unwrap())]
    #[case::empty_client(OidcProviderBuildError::EmptyClientId, |s: &mut OidcProviderSettings| s.client_id.clear())]
    #[case::empty_subject(OidcProviderBuildError::InvalidClaim, |s: &mut OidcProviderSettings| s.subject_claim.clear())]
    #[case::empty_display(OidcProviderBuildError::InvalidClaim, |s: &mut OidcProviderSettings| s.display_name_claim.clear())]
    #[case::empty_group(OidcProviderBuildError::InvalidClaim, |s: &mut OidcProviderSettings| s.groups_claim = Some(String::new()))]
    #[case::zero_timeout(OidcProviderBuildError::InvalidTimeout, |s: &mut OidcProviderSettings| s.request_timeout = Duration::ZERO)]
    fn test_new_rejects_invalid_settings(
        #[case] expected: OidcProviderBuildError,
        #[case] mutate: fn(&mut OidcProviderSettings),
    ) {
        let mut raw = settings("https://issuer.example");
        mutate(&mut raw);
        assert_eq!(OidcLoginProvider::new(raw).unwrap_err(), expected);
    }

    #[test]
    fn test_new_accepts_a_secure_provider() {
        let provider = OidcLoginProvider::new(settings("https://issuer.example")).unwrap();
        assert_eq!(provider.id().as_str(), "corporate");
    }

    #[rstest]
    #[case::unavailable(OidcProviderError::Unavailable, true)]
    #[case::invalid_response(OidcProviderError::InvalidProviderResponse, true)]
    #[case::unknown_key(OidcProviderError::UnknownKey, true)]
    #[case::state(OidcProviderError::StateMismatch, false)]
    #[case::exchange(OidcProviderError::TokenExchange, false)]
    #[case::token(OidcProviderError::InvalidToken, false)]
    #[case::claims(OidcProviderError::InvalidClaims, false)]
    fn test_error_availability(#[case] error: OidcProviderError, #[case] expected: bool) {
        assert_eq!(error.unavailable(), expected);
    }

    // One concrete store keeps the suite to a single OidcLoginService monomorphization; per-test
    // closure stores would leave each mono's uncalled methods counted as uncovered on x86.
    #[derive(Clone)]
    struct TestStore {
        outcome: Result<ExternalIdentityResolution, &'static str>,
        calls: Arc<Mutex<usize>>,
    }

    impl TestStore {
        fn ok() -> Self {
            Self {
                outcome: Ok(resolution()),
                calls: Arc::new(Mutex::new(0)),
            }
        }

        fn failing() -> Self {
            Self {
                outcome: Err("store down"),
                calls: Arc::new(Mutex::new(0)),
            }
        }

        fn calls(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    impl crate::ExternalIdentityStore for TestStore {
        type Error = &'static str;

        fn link_or_resolve(&self, _request: ExternalLinkRequest) -> Result<ExternalIdentityResolution, Self::Error> {
            *self.calls.lock().unwrap() += 1;
            self.outcome.clone()
        }
    }

    fn resolution() -> ExternalIdentityResolution {
        ExternalIdentityResolution {
            user: ServerUser {
                id: UserId::random(),
                name: UserName::new("Ada").unwrap(),
                state: UserState::Active,
                revision: 1,
            },
            link_created: true,
            grants_changed: false,
        }
    }

    fn service(issuer: &str, store: TestStore) -> OidcLoginService<TestStore> {
        OidcLoginService::new(provider(issuer), store, Vec::new())
    }

    #[test]
    fn test_debug_redacts_secrets() {
        let provider = provider("https://issuer.example");
        let rendered = format!("{provider:?}");
        assert!(rendered.contains("[redacted]"));
        assert!(!rendered.contains("s3cret"));
        assert_eq!(format!("{:?}", pending()), "PendingLogin([redacted])");
        let service = OidcLoginService::new(provider, TestStore::ok(), Vec::new());
        assert!(format!("{service:?}").contains("OidcLoginService"));
    }

    #[tokio::test]
    async fn test_service_authorizes_and_commits_the_link() {
        let server = MockServer::start().await;
        mount_metadata(&server, json!({"keys": [jwk("key-1")]})).await;
        mount_token(
            &server,
            json!({"id_token": mint("key-1", &base_claims(&server)), "token_type": "Bearer"}),
        )
        .await;
        let store = TestStore::ok();
        let service = service(&server.uri(), store.clone());
        assert_eq!(service.id().as_str(), "corporate");
        assert!(service.authorization(NOW).await.is_ok());
        let outcome = service.callback(&response(), &pending(), NOW).await.unwrap();
        assert!(outcome.link_created);
        assert_eq!(store.calls(), 1);
    }

    #[tokio::test]
    async fn test_service_reports_a_store_failure() {
        let server = MockServer::start().await;
        mount_metadata(&server, json!({"keys": [jwk("key-1")]})).await;
        mount_token(
            &server,
            json!({"id_token": mint("key-1", &base_claims(&server)), "token_type": "Bearer"}),
        )
        .await;
        let store = TestStore::failing();
        let service = service(&server.uri(), store.clone());
        assert!(matches!(
            service.callback(&response(), &pending(), NOW).await,
            Err(OidcLoginError::Store("store down"))
        ));
        assert_eq!(store.calls(), 1);
    }

    #[tokio::test]
    async fn test_service_propagates_a_provider_failure() {
        let server = MockServer::start().await;
        mount_metadata(&server, json!({"keys": [jwk("key-1")]})).await;
        mount_token(&server, json!({"token_type": "Bearer"})).await;
        let store = TestStore::ok();
        let service = service(&server.uri(), store.clone());
        assert!(matches!(
            service.callback(&response(), &pending(), NOW).await,
            Err(OidcLoginError::Provider(OidcProviderError::TokenExchange))
        ));
        assert_eq!(store.calls(), 0);
    }
}
