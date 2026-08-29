//! [`OidcLoginProvider`] handles browser logins; [`crate::oidc`] handles machine credentials. State,
//! nonce, and PKCE bind each browser attempt. The provider pins the configured issuer and caches signing
//! keys, so authenticated requests do not depend on provider availability.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use reqwest::header::CONTENT_TYPE;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tokio::sync::Mutex as AsyncMutex;
use url::Url;

use crate::oidc::Audience;
use crate::oidc_http::{
    BoundedResponseError, DISCOVERY_BODY_LIMIT, JWKS_BODY_LIMIT, MIN_FRESH_SECS, OidcHttpError, OidcHttpTransport,
    ReqwestOidcHttpTransport, discovery_url, fetch, fetch_bounded, freshness, usable_keys,
};
use crate::{
    ExternalGroup, ExternalGroupGrant, ExternalIdentity, ExternalIdentityLinker, ExternalIdentityResolution,
    ExternalIdentityStore, ExternalLogin, ExternalSubject, ProviderId, UserName,
};

const TOKEN_BODY_LIMIT: usize = 64 * 1024;
const HARD_CACHE_SECS: i64 = 3600;
const RANDOM_BYTES: usize = 32;
const OPENID_SCOPE: &str = "openid";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidcProviderSettings {
    pub id: ProviderId,
    /// The discovery document must repeat the configured issuer URL byte for byte.
    pub issuer: String,
    pub client_id: String,
    /// `None` configures a public client that relies on PKCE.
    pub client_secret: Option<String>,
    /// Authorization and exchange requests use this pre-registered callback URL.
    pub redirect_uri: Url,
    /// The provider adds `openid` when absent.
    pub scopes: Vec<String>,
    /// Claim containing the stable subject; providers tend to use `sub`.
    pub subject_claim: String,
    /// Claim containing the display name; providers tend to use `name`.
    pub display_name_claim: String,
    /// Use `None` when the provider asserts no groups.
    pub groups_claim: Option<String>,
    /// Tolerance applied to token time claims for provider clock drift.
    pub clock_skew: Duration,
    pub request_timeout: Duration,
}

/// Returns provider assertions without changing local state.
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
    client: reqwest::Client,
    transport: Arc<dyn OidcHttpTransport>,
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
    /// Validates inputs without opening a provider connection.
    ///
    /// # Errors
    /// Returns [`OidcProviderBuildError`] for an invalid issuer or redirect URL, an empty client ID or
    /// claim name, or a non-positive timeout.
    pub fn new(settings: OidcProviderSettings) -> Result<Self, OidcProviderBuildError> {
        let client = oidc_client(settings.request_timeout)?;
        Self::with_transport(settings, Arc::new(ReqwestOidcHttpTransport(client.clone())), client)
    }

    /// # Errors
    /// Returns the same validation errors as [`new`](Self::new).
    pub fn with_http_transport(
        settings: OidcProviderSettings,
        transport: Arc<dyn OidcHttpTransport>,
    ) -> Result<Self, OidcProviderBuildError> {
        let client = oidc_client(settings.request_timeout)?;
        Self::with_transport(settings, transport, client)
    }

    fn with_transport(
        settings: OidcProviderSettings,
        transport: Arc<dyn OidcHttpTransport>,
        client: reqwest::Client,
    ) -> Result<Self, OidcProviderBuildError> {
        let issuer = Url::parse(&settings.issuer).map_err(|_| OidcProviderBuildError::InvalidIssuer)?;
        let issuer = secure_url(&issuer).ok_or(OidcProviderBuildError::InvalidIssuer)?;
        if issuer.query().is_some()
            || issuer.fragment().is_some()
            || (issuer.as_str() != settings.issuer
                && !(issuer.path() == "/" && issuer[..url::Position::BeforePath] == settings.issuer))
        {
            return Err(OidcProviderBuildError::InvalidIssuer);
        }
        if secure_url(&settings.redirect_uri).is_none() || settings.redirect_uri.fragment().is_some() {
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
        Ok(Self {
            id: settings.id,
            issuer: settings.issuer,
            discovery: discovery_url(issuer),
            client_id: settings.client_id,
            client_secret: settings.client_secret,
            redirect_uri: settings.redirect_uri,
            scope: scope_string(&settings.scopes),
            subject_claim: settings.subject_claim,
            display_name_claim: settings.display_name_claim,
            groups_claim: settings.groups_claim,
            leeway_secs: settings.clock_skew.as_secs(),
            client,
            transport,
            cache: Arc::new(AsyncMutex::new(Cache::default())),
        })
    }

    #[must_use]
    pub const fn id(&self) -> &ProviderId {
        &self.id
    }

    /// Returns a redirect and fresh [`PendingLogin`]. Bind the pending state to the browser and return
    /// it once at callback.
    ///
    /// # Errors
    /// Returns [`OidcProviderError`] when provider discovery or validation fails.
    pub async fn authorization(&self, now: i64) -> Result<Authorization, OidcProviderError> {
        let endpoints = self.endpoints(now).await?;
        let verifier = random_token()?;
        let pending = PendingLogin {
            state: random_token()?,
            nonce: random_token()?,
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

    /// # Errors
    /// Returns [`OidcProviderError`] on a state mismatch, a failed exchange, or an ID token that fails
    /// signature, issuer, audience, time claim, or nonce validation.
    pub async fn callback(
        &self,
        response: &CallbackResponse,
        pending: &PendingLogin,
        now: i64,
    ) -> Result<ExternalLogin, OidcProviderError> {
        if !pending.matches_state(&response.state) {
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
        let response = fetch_bounded(self.transport.as_ref(), request, TOKEN_BODY_LIMIT)
            .await
            .map_err(|error| {
                OidcProviderError::TokenExchange(match error {
                    BoundedResponseError::Transport { status } => OidcTokenExchangeError::Transport { status },
                    BoundedResponseError::InvalidResponse { status } => {
                        OidcTokenExchangeError::InvalidResponse { status }
                    }
                })
            })?;
        let status = response.status.as_u16();
        if !response.json {
            return Err(OidcProviderError::TokenExchange(
                OidcTokenExchangeError::InvalidResponse { status },
            ));
        }
        if response.status.is_success() {
            return serde_json::from_slice::<TokenResponse>(&response.body)
                .map(|response| response.id_token)
                .map_err(|_| OidcProviderError::TokenExchange(OidcTokenExchangeError::InvalidResponse { status }));
        }
        Err(
            serde_json::from_slice::<TokenErrorResponse>(&response.body).map_or_else(
                |_| OidcProviderError::TokenExchange(OidcTokenExchangeError::InvalidResponse { status }),
                |response| {
                    OidcProviderError::TokenExchange(OidcTokenExchangeError::Protocol {
                        status,
                        code: token_error_code(&response.error),
                    })
                },
            ),
        )
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
        // Use the caller's clock for cache freshness and token validity instead of jsonwebtoken's wall clock.
        validation.validate_exp = false;
        validation.validate_nbf = false;
        validation.set_required_spec_claims(&["iss", "aud", "sub"]);
        validation.set_issuer(&[&self.issuer]);
        validation.set_audience(&[&self.client_id]);
        let claims = decode::<IdTokenClaims>(id_token, &key, &validation)
            .map_err(|_| OidcProviderError::InvalidToken)?
            .claims;
        let skew = i64::try_from(self.leeway_secs).unwrap_or(i64::MAX);
        if claims.exp.saturating_add(skew) <= now
            || claims.iat.saturating_sub(skew) > now
            || claims
                .nbf
                .is_some_and(|not_before| not_before.saturating_sub(skew) > now)
        {
            return Err(OidcProviderError::InvalidToken);
        }
        if !crate::secrets_match(&claims.nonce, nonce) {
            return Err(OidcProviderError::InvalidToken);
        }
        // OIDC Core §3.1.3.7 requires `azp` to name this client when a token has multiple audiences.
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
        let authorization = Self::endpoint(&discovery.authorization_endpoint)?;
        let token = Self::endpoint(&discovery.token_endpoint)?;
        let jwks_uri = Self::endpoint(&discovery.jwks_uri)?;
        let (jwks_body, jwks_age) = self.fetch(self.client.get(jwks_uri), JWKS_BODY_LIMIT).await?;
        let jwks =
            serde_json::from_slice::<JwkSet>(&jwks_body).map_err(|_| OidcProviderError::InvalidProviderResponse)?;
        let keys = usable_keys(jwks)?;
        let fresh_for = freshness(discovery_age, jwks_age);
        Ok(Cache {
            endpoints: Some(Endpoints { authorization, token }),
            keys,
            fresh_until: now + fresh_for,
            hard_until: now + HARD_CACHE_SECS,
            refresh_after: now + MIN_FRESH_SECS,
        })
    }

    fn endpoint(value: &str) -> Result<Url, OidcProviderError> {
        Url::parse(value)
            .ok()
            .filter(|url| secure_url(url).is_some())
            .ok_or(OidcProviderError::InvalidProviderResponse)
    }

    async fn fetch(
        &self,
        request: reqwest::RequestBuilder,
        limit: usize,
    ) -> Result<(Vec<u8>, Option<i64>), OidcProviderError> {
        fetch(self.transport.as_ref(), request, limit)
            .await
            .map_err(OidcProviderError::from)
    }
}

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

    /// # Errors
    /// Returns [`OidcProviderError`] when provider discovery or validation fails.
    pub async fn authorization(&self, now: i64) -> Result<Authorization, OidcProviderError> {
        self.provider.authorization(now).await
    }

    /// Commits local identity state after ID-token validation succeeds.
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authorization {
    pub redirect_url: Url,
    pub pending: PendingLogin,
}

/// Treat all fields as secrets; bind them to the browser session and use them once.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingLogin {
    pub state: String,
    pub nonce: String,
    pub verifier: String,
    pub challenge: String,
}

impl PendingLogin {
    #[must_use]
    pub fn matches_state(&self, presented: &str) -> bool {
        crate::secrets_match(presented, &self.state)
    }
}

impl std::fmt::Debug for PendingLogin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("PendingLogin([redacted])")
    }
}

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
pub enum OidcTokenExchangeError {
    #[error("OIDC token endpoint transport failed")]
    Transport { status: Option<u16> },
    #[error("OIDC token endpoint returned HTTP {status} with OAuth error {code:?}")]
    Protocol { status: u16, code: OidcTokenErrorCode },
    #[error("OIDC token endpoint returned an invalid HTTP {status} response")]
    InvalidResponse { status: u16 },
}

impl OidcTokenExchangeError {
    #[must_use]
    pub const fn authentication_rejected(&self) -> bool {
        matches!(
            self,
            Self::Protocol {
                status: 400,
                code: OidcTokenErrorCode::InvalidGrant
            }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OidcTokenErrorCode {
    InvalidRequest,
    InvalidClient,
    InvalidGrant,
    UnauthorizedClient,
    UnsupportedGrantType,
    InvalidScope,
    Unknown,
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
    #[error("OIDC authorization code exchange failed: {0}")]
    TokenExchange(OidcTokenExchangeError),
    #[error("OIDC ID token failed validation")]
    InvalidToken,
    #[error("OIDC ID token claims do not match the configured mapping")]
    InvalidClaims,
}

impl OidcProviderError {
    /// Distinguishes provider outages from rejected logins.
    #[must_use]
    pub const fn unavailable(&self) -> bool {
        matches!(
            self,
            Self::Unavailable
                | Self::InvalidProviderResponse
                | Self::UnknownKey
                | Self::TokenExchange(OidcTokenExchangeError::Transport { .. })
        )
    }

    #[must_use]
    pub const fn authentication_rejected(&self) -> bool {
        matches!(self, Self::TokenExchange(error) if error.authentication_rejected())
    }
}

impl From<OidcHttpError> for OidcProviderError {
    fn from(error: OidcHttpError) -> Self {
        match error {
            OidcHttpError::Unavailable => Self::Unavailable,
            OidcHttpError::InvalidResponse => Self::InvalidProviderResponse,
        }
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
struct TokenErrorResponse {
    error: String,
}

#[derive(Deserialize)]
struct IdTokenClaims {
    aud: Audience,
    exp: i64,
    iat: i64,
    #[serde(default)]
    nbf: Option<i64>,
    nonce: String,
    #[serde(default)]
    azp: Option<String>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

fn random_token() -> Result<String, OidcProviderError> {
    let mut bytes = [0u8; RANDOM_BYTES];
    getrandom::fill(&mut bytes).map_err(|_| OidcProviderError::Unavailable)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn token_error_code(error: &str) -> OidcTokenErrorCode {
    match error {
        "invalid_request" => OidcTokenErrorCode::InvalidRequest,
        "invalid_client" => OidcTokenErrorCode::InvalidClient,
        "invalid_grant" => OidcTokenErrorCode::InvalidGrant,
        "unauthorized_client" => OidcTokenErrorCode::UnauthorizedClient,
        "unsupported_grant_type" => OidcTokenErrorCode::UnsupportedGrantType,
        "invalid_scope" => OidcTokenErrorCode::InvalidScope,
        _ => OidcTokenErrorCode::Unknown,
    }
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
    let values = match value {
        Value::String(_) => std::slice::from_ref(value),
        Value::Array(values) => values,
        _ => return Err(OidcProviderError::InvalidClaims),
    };
    values
        .iter()
        .map(|value| {
            ExternalGroup::new(value.as_str().ok_or(OidcProviderError::InvalidClaims)?)
                .map_err(|_| OidcProviderError::InvalidClaims)
        })
        .collect()
}

fn secure_url(url: &Url) -> Option<&Url> {
    (url.scheme() == "https" && url.host_str().is_some() && url.username().is_empty() && url.password().is_none())
        .then_some(url)
}

fn oidc_client(request_timeout: Duration) -> Result<reqwest::Client, OidcProviderBuildError> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(request_timeout)
        .build()
        .or(Err(OidcProviderBuildError::InvalidTimeout))
}

#[cfg(test)]
#[path = "../tests/unit/oidc_login/tests.rs"]
mod tests;
