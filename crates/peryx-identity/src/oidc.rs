//! Verification and exchange of short-lived CI identities.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use jsonwebtoken::jwk::{AlgorithmParameters, JwkSet, KeyOperations, PublicKeyUse};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use reqwest::header::{CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex as AsyncMutex;
use url::Url;

use crate::trusted_publisher::authorize_publish_index;
use crate::{Glob, Grant, Principal, PublishClaims, PublishDenial, Signer, TrustedPublisher};

const DISCOVERY_BODY_LIMIT: usize = 64 * 1024;
const JWKS_BODY_LIMIT: usize = 1024 * 1024;
const TOKEN_BODY_LIMIT: usize = 32 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MIN_FRESH_SECS: i64 = 60;
const DEFAULT_FRESH_SECS: i64 = 300;
const MAX_FRESH_SECS: i64 = 900;
const HARD_CACHE_SECS: i64 = 3600;
const MAX_IDENTITY_LIFETIME_SECS: i64 = 3600;
const MAX_REPLAY_ENTRIES: usize = 65_536;
const MAX_JTI_BYTES: usize = 256;
const MAX_SUBJECT_BYTES: usize = 2048;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublisherBinding {
    pub id: String,
    pub repository: String,
    pub publisher: TrustedPublisher,
}

#[async_trait]
pub trait IdentityExchange: Send + Sync {
    fn audience(&self) -> &str;

    async fn exchange(&self, token: &str, now: i64) -> Result<ExchangedToken, ExchangeError>;
}

pub struct OidcRuntime {
    audience: String,
    bindings: Vec<PublisherBinding>,
    publishers: Vec<TrustedPublisher>,
    issuers: HashMap<String, Arc<IssuerState>>,
    client: reqwest::Client,
    signer: Signer,
    token_ttl_secs: i64,
    replay: Mutex<HashMap<(String, String), i64>>,
    replay_capacity: usize,
}

impl OidcRuntime {
    /// # Errors
    /// Rejects an empty publisher set, inconsistent audiences, invalid issuer URLs, and duplicate IDs.
    pub fn new(bindings: Vec<PublisherBinding>, signer: Signer, token_ttl_secs: i64) -> Result<Self, ExchangeError> {
        Self::build(bindings, signer, token_ttl_secs, false, MAX_REPLAY_ENTRIES)
    }

    fn build(
        bindings: Vec<PublisherBinding>,
        signer: Signer,
        token_ttl_secs: i64,
        allow_insecure_issuers: bool,
        replay_capacity: usize,
    ) -> Result<Self, ExchangeError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        if token_ttl_secs <= 0 || replay_capacity == 0 {
            return Err(ExchangeError::Configuration);
        }
        let first = bindings.first().ok_or(ExchangeError::Configuration)?;
        let audience = first.publisher.audience.clone();
        let mut ids = HashSet::new();
        let mut issuers = HashMap::new();
        for binding in &bindings {
            if binding.id.trim().is_empty()
                || binding.repository.contains("..")
                || binding.publisher.audience != audience
                || !ids.insert(binding.id.clone())
            {
                return Err(ExchangeError::Configuration);
            }
            let issuer = issuer_url(&binding.publisher.issuer, allow_insecure_issuers)?;
            issuers.entry(binding.publisher.issuer.clone()).or_insert_with(|| {
                Arc::new(IssuerState {
                    issuer: binding.publisher.issuer.clone(),
                    discovery: discovery_url(&issuer),
                    cache: AsyncMutex::new(KeyCache::default()),
                })
            });
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(REQUEST_TIMEOUT)
            .build()
            .or(Err(ExchangeError::Configuration))?;
        let publishers = bindings.iter().map(|binding| binding.publisher.clone()).collect();
        Ok(Self {
            audience,
            bindings,
            publishers,
            issuers,
            client,
            signer,
            token_ttl_secs,
            replay: Mutex::new(HashMap::new()),
            replay_capacity,
        })
    }

    #[must_use]
    pub fn audience(&self) -> &str {
        &self.audience
    }

    /// # Errors
    /// Fails closed for malformed, unverifiable, unauthorized, expired, or replayed identities.
    pub async fn exchange(&self, token: &str, now: i64) -> Result<ExchangedToken, ExchangeError> {
        let verified = self.verify(token, now).await?;
        let (position, mut grants) = authorize_publish_index(&self.publishers, &verified.publish, now)?;
        let binding = &self.bindings[position];
        qualify_grants(&mut grants, &binding.repository);
        let ttl_secs = self.token_ttl_secs.min(verified.publish.expires_at - now);
        let token_id = uuid::Uuid::new_v4().to_string();
        let principal = Principal::Named {
            subject: format!("trusted-publisher:{}", binding.id),
        };
        let token = self.signer.mint_trusted(&principal, &grants, now, ttl_secs, &token_id);
        self.consume_replay(
            &verified.publish.issuer,
            &verified.jti,
            verified.publish.expires_at,
            now,
        )?;
        Ok(ExchangedToken {
            token,
            token_id,
            publisher_id: binding.id.clone(),
            repository: binding.repository.clone(),
            expires_at: now + ttl_secs,
        })
    }

    async fn verify(&self, token: &str, now: i64) -> Result<VerifiedIdentity, ExchangeError> {
        if token.len() > TOKEN_BODY_LIMIT {
            return Err(ExchangeError::InvalidIdentity);
        }
        let unverified = jsonwebtoken::dangerous::insecure_decode::<ExternalClaims>(token)
            .map_err(|_| ExchangeError::InvalidIdentity)?;
        if unverified.header.alg != Algorithm::RS256 {
            return Err(ExchangeError::InvalidIdentity);
        }
        let kid = unverified
            .header
            .kid
            .as_deref()
            .filter(|kid| !kid.is_empty())
            .ok_or(ExchangeError::InvalidIdentity)?;
        let state = self
            .issuers
            .get(&unverified.claims.iss)
            .ok_or(ExchangeError::InvalidIdentity)?;
        let key = self.key(state, kid, now).await?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.leeway = 0;
        validation.validate_exp = false;
        validation.validate_nbf = false;
        validation.set_required_spec_claims(&["iss", "aud", "sub", "exp"]);
        validation.set_issuer(&[&unverified.claims.iss]);
        validation.set_audience(&[&self.audience]);
        let claims = jsonwebtoken::decode::<ExternalClaims>(token, &key, &validation)
            .map_err(|_| ExchangeError::InvalidIdentity)?
            .claims;
        let audience = claims.aud.one().ok_or(ExchangeError::InvalidIdentity)?;
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
            return Err(ExchangeError::InvalidIdentity);
        }
        let extra = claims
            .extra
            .into_iter()
            .filter_map(|(name, value)| value.as_str().map(|value| (name, value.to_owned())))
            .collect();
        Ok(VerifiedIdentity {
            publish: PublishClaims {
                issuer: claims.iss,
                audience: audience.to_owned(),
                subject: claims.sub,
                expires_at: claims.exp,
                claims: extra,
            },
            jti: claims.jti,
        })
    }

    async fn key(&self, state: &IssuerState, kid: &str, now: i64) -> Result<DecodingKey, ExchangeError> {
        let mut cache = state.cache.lock().await;
        let cached = cache.key(kid);
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
        cache.key(kid).ok_or(ExchangeError::UnknownKey)
    }

    async fn refresh(&self, state: &IssuerState, now: i64) -> Result<KeyCache, ExchangeError> {
        let (discovery, discovery_age) = self
            .fetch_json::<Discovery>(&state.discovery, DISCOVERY_BODY_LIMIT)
            .await?;
        if discovery.issuer != state.issuer || !discovery.algorithms.iter().any(|algorithm| algorithm == "RS256") {
            return Err(ExchangeError::InvalidIssuerResponse);
        }
        let jwks_uri = issuer_url(&discovery.jwks_uri, state.discovery.scheme() == "http")
            .map_err(|_| ExchangeError::InvalidIssuerResponse)?;
        let (jwks, jwks_age) = self.fetch_json::<JwkSet>(&jwks_uri, JWKS_BODY_LIMIT).await?;
        let keys = usable_keys(jwks)?;
        let fresh_for = discovery_age
            .unwrap_or(DEFAULT_FRESH_SECS)
            .min(jwks_age.unwrap_or(DEFAULT_FRESH_SECS))
            .clamp(MIN_FRESH_SECS, MAX_FRESH_SECS);
        Ok(KeyCache {
            keys,
            fresh_until: now + fresh_for,
            hard_until: now + HARD_CACHE_SECS,
            refresh_after: now + MIN_FRESH_SECS,
        })
    }

    async fn fetch_json<T: for<'de> Deserialize<'de>>(
        &self,
        url: &Url,
        limit: usize,
    ) -> Result<(T, Option<i64>), ExchangeError> {
        let mut response = self
            .client
            .get(url.clone())
            .send()
            .await
            .map_err(|_| ExchangeError::IssuerUnavailable)?;
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
            return Err(ExchangeError::InvalidIssuerResponse);
        }
        let max_age = response
            .headers()
            .get(CACHE_CONTROL)
            .and_then(|value| value.to_str().ok())
            .and_then(cache_max_age);
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| ExchangeError::IssuerUnavailable)? {
            if body.len() + chunk.len() > limit {
                return Err(ExchangeError::InvalidIssuerResponse);
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body)
            .map(|value| (value, max_age))
            .map_err(|_| ExchangeError::InvalidIssuerResponse)
    }

    fn consume_replay(&self, issuer: &str, jti: &str, expires_at: i64, now: i64) -> Result<(), ExchangeError> {
        let mut replay = self.replay.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        replay.retain(|_, expiry| *expiry > now);
        let key = (issuer.to_owned(), jti.to_owned());
        if replay.contains_key(&key) {
            return Err(ExchangeError::Replay);
        }
        if replay.len() >= self.replay_capacity {
            return Err(ExchangeError::ReplayCapacity);
        }
        replay.insert(key, expires_at);
        drop(replay);
        Ok(())
    }
}

#[async_trait]
impl IdentityExchange for OidcRuntime {
    fn audience(&self) -> &str {
        self.audience()
    }

    async fn exchange(&self, token: &str, now: i64) -> Result<ExchangedToken, ExchangeError> {
        Self::exchange(self, token, now).await
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ExchangedToken {
    pub token: String,
    pub token_id: String,
    pub publisher_id: String,
    pub repository: String,
    pub expires_at: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum ExchangeError {
    #[error("trusted publishing is misconfigured")]
    Configuration,
    #[error("the identity token is invalid")]
    InvalidIdentity,
    #[error("the issuer is unavailable")]
    IssuerUnavailable,
    #[error("the issuer returned an invalid response")]
    InvalidIssuerResponse,
    #[error("the identity token names an unknown signing key")]
    UnknownKey,
    #[error("the identity token has already been exchanged")]
    Replay,
    #[error("the identity replay cache is full")]
    ReplayCapacity,
    #[error(transparent)]
    Denied(#[from] PublishDenial),
}

impl ExchangeError {
    #[must_use]
    pub const fn unavailable(&self) -> bool {
        matches!(
            self,
            Self::IssuerUnavailable | Self::InvalidIssuerResponse | Self::UnknownKey | Self::ReplayCapacity
        )
    }
}

struct IssuerState {
    issuer: String,
    discovery: Url,
    cache: AsyncMutex<KeyCache>,
}

#[derive(Default)]
struct KeyCache {
    keys: HashMap<String, DecodingKey>,
    fresh_until: i64,
    hard_until: i64,
    refresh_after: i64,
}

impl KeyCache {
    fn key(&self, kid: &str) -> Option<DecodingKey> {
        self.keys.get(kid).cloned()
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

    /// Whether the token names more than one audience, so OIDC Core §3.1.3.7 requires an `azp` claim.
    pub const fn is_multiple(&self) -> bool {
        matches!(self, Self::Many(values) if values.len() > 1)
    }
}

struct VerifiedIdentity {
    publish: PublishClaims,
    jti: String,
}

fn issuer_url(value: &str, allow_insecure: bool) -> Result<Url, ExchangeError> {
    let url = Url::parse(value).map_err(|_| ExchangeError::Configuration)?;
    if (!allow_insecure && url.scheme() != "https")
        || (allow_insecure && !matches!(url.scheme(), "http" | "https"))
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ExchangeError::Configuration);
    }
    Ok(url)
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

fn qualify_grants(grants: &mut [Grant], repository: &str) {
    if repository.is_empty() {
        return;
    }
    for grant in grants {
        for project in &mut grant.projects {
            *project = Glob::new(format!("{repository}/{}", project.as_str()));
        }
    }
}

fn usable_keys(jwks: JwkSet) -> Result<HashMap<String, DecodingKey>, ExchangeError> {
    let mut ids = HashSet::new();
    if jwks.keys.is_empty()
        || jwks.keys.iter().any(|key| {
            let Some(id) = key.common.key_id.as_deref().filter(|id| !id.is_empty()) else {
                return true;
            };
            !ids.insert(id)
        })
    {
        return Err(ExchangeError::InvalidIssuerResponse);
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
        let decoding = DecodingKey::from_jwk(&key).map_err(|_| ExchangeError::InvalidIssuerResponse)?;
        keys.insert(id, decoding);
    }
    if keys.is_empty() {
        return Err(ExchangeError::InvalidIssuerResponse);
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
