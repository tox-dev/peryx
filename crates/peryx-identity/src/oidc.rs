use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;
use url::Url;

use crate::oidc_http::{
    CachePolicy, CacheWindow, DISCOVERY_BODY_LIMIT, JWKS_BODY_LIMIT, OidcHttpError, OidcHttpTransport,
    REFRESH_BACKOFF_SECS, discovery_url, fetch, usable_keys,
};

const TOKEN_BODY_LIMIT: usize = 32 * 1024;
const MAX_IDENTITY_LIFETIME_SECS: i64 = 3600;
const MAX_JTI_BYTES: usize = 256;
const MAX_SUBJECT_BYTES: usize = 2048;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedOidcIdentity {
    pub issuer: String,
    pub audience: String,
    pub subject: String,
    pub expires_at: i64,
    pub token_id: String,
    pub claims: BTreeMap<String, Value>,
}

#[async_trait]
pub trait OidcTokenVerifier: Send + Sync {
    async fn verify(
        &self,
        token: &str,
        expected_audience: &str,
        now: i64,
    ) -> Result<VerifiedOidcIdentity, OidcVerificationError>;
}

pub struct OidcVerifier {
    audience: String,
    issuers: HashMap<String, Arc<IssuerState>>,
    transport: Arc<dyn OidcHttpTransport>,
}

impl OidcVerifier {
    /// `transport` carries the deployment's outbound destination policy; the verifier opens no
    /// connection of its own.
    ///
    /// # Errors
    /// Rejects an empty audience or an invalid issuer URL.
    pub fn new(
        issuers: impl IntoIterator<Item = String>,
        audience: impl Into<String>,
        transport: Arc<dyn OidcHttpTransport>,
    ) -> Result<Self, OidcVerificationError> {
        let audience = audience.into();
        if audience.trim().is_empty() {
            return Err(OidcVerificationError::Configuration);
        }
        let mut states = HashMap::new();
        for issuer in issuers {
            let url = issuer_url(&issuer)?;
            states.entry(issuer.clone()).or_insert_with(|| {
                Arc::new(IssuerState {
                    issuer,
                    discovery: discovery_url(&url),
                    cache: Mutex::new(KeyCache::default()),
                })
            });
        }
        if states.is_empty() {
            return Err(OidcVerificationError::Configuration);
        }
        Ok(Self {
            audience,
            issuers: states,
            transport,
        })
    }

    async fn verify_token(&self, token: &str, now: i64) -> Result<VerifiedOidcIdentity, OidcVerificationError> {
        if token.len() > TOKEN_BODY_LIMIT {
            return Err(OidcVerificationError::InvalidIdentity);
        }
        let unverified = jsonwebtoken::dangerous::insecure_decode::<ExternalClaims>(token)
            .or(Err(OidcVerificationError::InvalidIdentity))?;
        if unverified.header.alg != Algorithm::RS256 {
            return Err(OidcVerificationError::InvalidIdentity);
        }
        let key_id = unverified
            .header
            .kid
            .as_deref()
            .filter(|key_id| !key_id.is_empty())
            .ok_or(OidcVerificationError::InvalidIdentity)?;
        let state = self
            .issuers
            .get(&unverified.claims.iss)
            .ok_or(OidcVerificationError::InvalidIdentity)?;
        let key = self.key(state, key_id, now).await?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.leeway = 0;
        validation.validate_exp = false;
        validation.validate_nbf = false;
        validation.set_required_spec_claims(&["iss", "aud", "sub", "exp"]);
        validation.set_issuer(&[&unverified.claims.iss]);
        validation.set_audience(&[&self.audience]);
        let claims = jsonwebtoken::decode::<ExternalClaims>(token, &key, &validation)
            .map_err(|_| OidcVerificationError::InvalidIdentity)?
            .claims;
        let audience = claims.aud.one().ok_or(OidcVerificationError::InvalidIdentity)?;
        if claims.sub.is_empty()
            || claims.sub.len() > MAX_SUBJECT_BYTES
            || claims.jti.is_empty()
            || claims.jti.len() > MAX_JTI_BYTES
            || claims.iat > now
            || claims.nbf.is_some_and(|not_before| now < not_before)
            || claims
                .exp
                .checked_sub(claims.iat)
                .is_none_or(|lifetime| lifetime <= 0 || lifetime > MAX_IDENTITY_LIFETIME_SECS)
        {
            return Err(OidcVerificationError::InvalidIdentity);
        }
        Ok(VerifiedOidcIdentity {
            issuer: claims.iss,
            audience: audience.to_owned(),
            subject: claims.sub,
            expires_at: claims.exp,
            token_id: claims.jti,
            claims: claims.extra,
        })
    }

    async fn key(&self, state: &IssuerState, key_id: &str, now: i64) -> Result<DecodingKey, OidcVerificationError> {
        let mut cache = state.cache.lock().await;
        if now < cache.window.fresh_until
            && let Some(key) = cache.key(key_id)
        {
            return Ok(key);
        }
        if now < cache.refresh_after {
            return cache.stale(key_id, now, OidcVerificationError::IssuerUnavailable);
        }
        match self.refresh(state, now).await {
            Ok((keys, window)) => {
                let key = keys.get(key_id).cloned();
                *cache = KeyCache::stored(keys, window, now);
                key.ok_or(OidcVerificationError::UnknownKey)
            }
            Err(error) => {
                cache.refresh_after = now + REFRESH_BACKOFF_SECS;
                cache.stale(key_id, now, error)
            }
        }
    }

    async fn refresh(
        &self,
        state: &IssuerState,
        now: i64,
    ) -> Result<(HashMap<String, DecodingKey>, CacheWindow), OidcVerificationError> {
        let (discovery, discovery_policy) = self
            .fetch_json::<Discovery>(&state.discovery, DISCOVERY_BODY_LIMIT)
            .await?;
        if discovery.issuer != state.issuer || !discovery.algorithms.iter().any(|algorithm| algorithm == "RS256") {
            return Err(OidcVerificationError::InvalidIssuerResponse);
        }
        let jwks_uri = issuer_url(&discovery.jwks_uri).or(Err(OidcVerificationError::InvalidIssuerResponse))?;
        if !self.transport.permits(&jwks_uri) {
            return Err(OidcVerificationError::BlockedDestination);
        }
        let (jwks, jwks_policy) = self.fetch_json::<JwkSet>(&jwks_uri, JWKS_BODY_LIMIT).await?;
        Ok((usable_keys(jwks)?, discovery_policy.strictest(&jwks_policy).window(now)))
    }

    async fn fetch_json<T: for<'de> Deserialize<'de>>(
        &self,
        url: &Url,
        limit: usize,
    ) -> Result<(T, CachePolicy), OidcVerificationError> {
        let request = self.transport.client().get(url.clone());
        let (body, policy) = fetch(self.transport.as_ref(), request, limit)
            .await
            .map_err(OidcVerificationError::from)?;
        serde_json::from_slice(&body)
            .map(|value| (value, policy))
            .map_err(|_| OidcVerificationError::InvalidIssuerResponse)
    }
}

#[async_trait]
impl OidcTokenVerifier for OidcVerifier {
    async fn verify(
        &self,
        token: &str,
        expected_audience: &str,
        now: i64,
    ) -> Result<VerifiedOidcIdentity, OidcVerificationError> {
        if expected_audience != self.audience {
            return Err(OidcVerificationError::InvalidIdentity);
        }
        self.verify_token(token, now).await
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OidcVerificationError {
    #[error("OIDC verification is misconfigured")]
    Configuration,
    #[error("the identity token is invalid")]
    InvalidIdentity,
    #[error("the issuer is unavailable")]
    IssuerUnavailable,
    #[error("the issuer returned an invalid response")]
    InvalidIssuerResponse,
    #[error("the issuer named a destination the outbound policy refuses")]
    BlockedDestination,
    #[error("the identity token names an unknown signing key")]
    UnknownKey,
}

impl OidcVerificationError {
    #[must_use]
    pub const fn unavailable(&self) -> bool {
        matches!(
            self,
            Self::IssuerUnavailable | Self::InvalidIssuerResponse | Self::BlockedDestination | Self::UnknownKey
        )
    }
}

impl From<OidcHttpError> for OidcVerificationError {
    fn from(error: OidcHttpError) -> Self {
        match error {
            OidcHttpError::Unavailable => Self::IssuerUnavailable,
            OidcHttpError::InvalidResponse => Self::InvalidIssuerResponse,
        }
    }
}

struct IssuerState {
    issuer: String,
    discovery: Url,
    cache: Mutex<KeyCache>,
}

#[derive(Default)]
struct KeyCache {
    keys: HashMap<String, DecodingKey>,
    window: CacheWindow,
    refresh_after: i64,
}

impl KeyCache {
    /// A response the provider forbids storing answers only the operation that fetched it, so it
    /// leaves an empty cache behind rather than a stale one the provider never authorized.
    fn stored(keys: HashMap<String, DecodingKey>, window: CacheWindow, now: i64) -> Self {
        if !window.storable {
            return Self::default();
        }
        Self {
            keys,
            // An unknown key refetches at most once per backoff, and never past the granted freshness.
            refresh_after: window.fresh_until.min(now + REFRESH_BACKOFF_SECS),
            window,
        }
    }

    /// A cached key answers a failed or suppressed refresh only inside the hard-stale window.
    fn stale(
        &self,
        key_id: &str,
        now: i64,
        unavailable: OidcVerificationError,
    ) -> Result<DecodingKey, OidcVerificationError> {
        if now >= self.window.hard_until {
            return Err(unavailable);
        }
        self.key(key_id).ok_or(OidcVerificationError::UnknownKey)
    }

    fn key(&self, key_id: &str) -> Option<DecodingKey> {
        self.keys.get(key_id).cloned()
    }
}

#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    jwks_uri: String,
    #[serde(default, rename = "id_token_signing_alg_values_supported")]
    algorithms: Vec<String>,
}

#[derive(Deserialize)]
struct ExternalClaims {
    iss: String,
    aud: Audience,
    sub: String,
    exp: i64,
    iat: i64,
    #[serde(default)]
    nbf: Option<i64>,
    jti: String,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub enum Audience {
    One(String),
    Many(Vec<String>),
}

impl Audience {
    fn one(&self) -> Option<&str> {
        match self {
            Self::One(value) => Some(value),
            Self::Many(values) if values.len() == 1 => values.first().map(String::as_str),
            Self::Many(_) => None,
        }
    }

    pub const fn is_multiple(&self) -> bool {
        matches!(self, Self::Many(values) if values.len() > 1)
    }
}

fn issuer_url(value: &str) -> Result<Url, OidcVerificationError> {
    let url = Url::parse(value).map_err(|_| OidcVerificationError::Configuration)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(OidcVerificationError::Configuration);
    }
    Ok(url)
}

#[cfg(test)]
#[path = "../tests/unit/oidc/tests.rs"]
mod tests;
