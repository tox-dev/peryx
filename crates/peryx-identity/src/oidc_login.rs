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

use crate::oidc::Audience;
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

    /// Build a login client that accepts an `http` issuer and redirect URI, so a test can point the flow
    /// at a local mock provider. Available only under `test-util`; production always pins `https`.
    ///
    /// # Errors
    /// Same validation as [`new`](Self::new), minus the transport-security check.
    #[cfg(feature = "test-util")]
    pub fn insecure(settings: OidcProviderSettings) -> Result<Self, OidcProviderBuildError> {
        Self::build(settings, true)
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
        // `set_audience` accepts any token that lists the client among its audiences; OIDC Core §3.1.3.7
        // additionally requires an `azp` claim once the token names more than one, and that it name this
        // client. Without this a token minted for another relying party that also lists us is accepted.
        if claims.aud.is_multiple() && claims.azp.is_none() {
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
    aud: Audience,
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
#[path = "../tests/unit/oidc_login/tests.rs"]
mod tests;
