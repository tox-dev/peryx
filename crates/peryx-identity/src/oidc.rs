use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use jsonwebtoken::jwk::{AlgorithmParameters, JwkSet, KeyOperations, PublicKeyUse};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use reqwest::Client;
use reqwest::header::{CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;
use url::Url;

use crate::oidc_http::{OidcHttpTransport, ReqwestOidcHttpTransport};

const DISCOVERY_BODY_LIMIT: usize = 64 * 1024;
const JWKS_BODY_LIMIT: usize = 1024 * 1024;
const TOKEN_BODY_LIMIT: usize = 32 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MIN_FRESH_SECS: i64 = 60;
const DEFAULT_FRESH_SECS: i64 = 300;
const MAX_FRESH_SECS: i64 = 900;
const HARD_CACHE_SECS: i64 = 3600;
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
    client: Client,
    transport: Arc<dyn OidcHttpTransport>,
}

impl OidcVerifier {
    /// # Errors
    /// Rejects an empty audience or an invalid issuer URL.
    pub fn new(
        issuers: impl IntoIterator<Item = String>,
        audience: impl Into<String>,
    ) -> Result<Self, OidcVerificationError> {
        let client = oidc_client()?;
        Self::with_transport(
            issuers.into_iter().collect(),
            audience,
            Arc::new(ReqwestOidcHttpTransport(client.clone())),
            client,
        )
    }

    /// # Errors
    /// Rejects an empty audience or an invalid issuer URL.
    pub fn with_http_transport(
        issuers: impl IntoIterator<Item = String>,
        audience: impl Into<String>,
        transport: Arc<dyn OidcHttpTransport>,
    ) -> Result<Self, OidcVerificationError> {
        Self::with_transport(issuers.into_iter().collect(), audience, transport, oidc_client()?)
    }

    fn with_transport(
        issuers: Vec<String>,
        audience: impl Into<String>,
        transport: Arc<dyn OidcHttpTransport>,
        client: Client,
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
            client,
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
        let cached = cache.key(key_id);
        let refresh = cached.is_none() || now >= cache.fresh_until;
        let hard_expired = !cache.keys.is_empty() && now >= cache.hard_until;
        if refresh && (now >= cache.refresh_after || hard_expired) {
            match self.refresh(state, now).await {
                Ok(next) => *cache = next,
                Err(error) => {
                    cache.refresh_after = now + MIN_FRESH_SECS;
                    if cached.is_none() || hard_expired {
                        return Err(error);
                    }
                }
            }
        }
        cache.key(key_id).ok_or(OidcVerificationError::UnknownKey)
    }

    async fn refresh(&self, state: &IssuerState, now: i64) -> Result<KeyCache, OidcVerificationError> {
        let (discovery, discovery_age) = self
            .fetch_json::<Discovery>(&state.discovery, DISCOVERY_BODY_LIMIT)
            .await?;
        if discovery.issuer != state.issuer || !discovery.algorithms.iter().any(|algorithm| algorithm == "RS256") {
            return Err(OidcVerificationError::InvalidIssuerResponse);
        }
        let jwks_uri = issuer_url(&discovery.jwks_uri).or(Err(OidcVerificationError::InvalidIssuerResponse))?;
        let (jwks, jwks_age) = self.fetch_json::<JwkSet>(&jwks_uri, JWKS_BODY_LIMIT).await?;
        let fresh_for = discovery_age
            .unwrap_or(DEFAULT_FRESH_SECS)
            .min(jwks_age.unwrap_or(DEFAULT_FRESH_SECS))
            .clamp(MIN_FRESH_SECS, MAX_FRESH_SECS);
        Ok(KeyCache {
            keys: usable_keys(jwks)?,
            fresh_until: now + fresh_for,
            hard_until: now + HARD_CACHE_SECS,
            refresh_after: now + MIN_FRESH_SECS,
        })
    }

    async fn fetch_json<T: for<'de> Deserialize<'de>>(
        &self,
        url: &Url,
        limit: usize,
    ) -> Result<(T, Option<i64>), OidcVerificationError> {
        let request = self
            .client
            .get(url.clone())
            .build()
            .or(Err(OidcVerificationError::IssuerUnavailable))?;
        let mut response = self
            .transport
            .execute(request)
            .await
            .or(Err(OidcVerificationError::IssuerUnavailable))?;
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
            return Err(OidcVerificationError::InvalidIssuerResponse);
        }
        let max_age = response
            .headers()
            .get(CACHE_CONTROL)
            .and_then(|value| value.to_str().ok())
            .and_then(cache_max_age);
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| OidcVerificationError::IssuerUnavailable)?
        {
            if body.len() + chunk.len() > limit {
                return Err(OidcVerificationError::InvalidIssuerResponse);
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body)
            .map(|value| (value, max_age))
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
    #[error("the identity token names an unknown signing key")]
    UnknownKey,
}

impl OidcVerificationError {
    #[must_use]
    pub const fn unavailable(&self) -> bool {
        matches!(
            self,
            Self::IssuerUnavailable | Self::InvalidIssuerResponse | Self::UnknownKey
        )
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
    fresh_until: i64,
    hard_until: i64,
    refresh_after: i64,
}

impl KeyCache {
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

fn oidc_client() -> Result<Client, OidcVerificationError> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(REQUEST_TIMEOUT)
        .build()
        .or(Err(OidcVerificationError::Configuration))
}

fn discovery_url(issuer: &Url) -> Url {
    let mut discovery = issuer.clone();
    discovery.set_path(&format!(
        "{}/.well-known/openid-configuration",
        issuer.path().trim_end_matches('/')
    ));
    discovery
}

fn usable_keys(jwks: JwkSet) -> Result<HashMap<String, DecodingKey>, OidcVerificationError> {
    let mut ids = HashSet::new();
    if jwks.keys.is_empty()
        || jwks.keys.iter().any(|key| {
            let Some(id) = key.common.key_id.as_deref().filter(|id| !id.is_empty()) else {
                return true;
            };
            !ids.insert(id)
        })
    {
        return Err(OidcVerificationError::InvalidIssuerResponse);
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
        let id = key.common.key_id.clone().expect("key IDs were validated");
        keys.insert(
            id,
            DecodingKey::from_jwk(&key).map_err(|_| OidcVerificationError::InvalidIssuerResponse)?,
        );
    }
    if keys.is_empty() {
        return Err(OidcVerificationError::InvalidIssuerResponse);
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
#[path = "../tests/unit/oidc/tests.rs"]
mod tests;
