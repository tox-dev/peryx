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

    #[cfg(test)]
    fn new_insecure(
        bindings: Vec<PublisherBinding>,
        signer: Signer,
        token_ttl_secs: i64,
    ) -> Result<Self, ExchangeError> {
        Self::build(bindings, signer, token_ttl_secs, true, MAX_REPLAY_ENTRIES)
    }

    #[cfg(test)]
    fn new_insecure_with_replay_capacity(
        bindings: Vec<PublisherBinding>,
        signer: Signer,
        token_ttl_secs: i64,
        replay_capacity: usize,
    ) -> Result<Self, ExchangeError> {
        Self::build(bindings, signer, token_ttl_secs, true, replay_capacity)
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
enum Audience {
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
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use jsonwebtoken::{Algorithm, EncodingKey, Header};
    use rstest::rstest;
    use serde::Serialize;
    use serde_json::{Value, json};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const NOW: i64 = 2_000_000_000;
    const MODULUS: &str = "yRE6rHuNR0QbHO3H3Kt2pOKGVhQqGZXInOduQNxXzuKlvQTLUTv4l4sggh5_CYYi_cvI-SXVT9kPWSKXxJXBXd_4LkvcPuUakBoAkfh-eiFVMh2VrUyWyj3MFl0HTVF9KwRXLAcwkREiS3npThHRyIxuy0ZMeZfxVL5arMhw1SRELB8HoGfG_AtH89BIE9jDBHZ9dLelK9a184zAf8LwoPLxvJb3Il5nncqPcSfKDDodMFBIMc4lQzDKL5gvmiXLXB1AGLm8KBjfE8s3L5xqi-yUod-j8MtvIj812dkS4QMiRVN_by2h3ZY8LYVGrqZXZTcgn2ujn8uKjXLZVD5TdQ";
    const PRIVATE_KEY_DER: &str = "MIIEpAIBAAKCAQEAyRE6rHuNR0QbHO3H3Kt2pOKGVhQqGZXInOduQNxXzuKlvQTLUTv4l4sggh5/CYYi/cvI+SXVT9kPWSKXxJXBXd/4LkvcPuUakBoAkfh+eiFVMh2VrUyWyj3MFl0HTVF9KwRXLAcwkREiS3npThHRyIxuy0ZMeZfxVL5arMhw1SRELB8HoGfG/AtH89BIE9jDBHZ9dLelK9a184zAf8LwoPLxvJb3Il5nncqPcSfKDDodMFBIMc4lQzDKL5gvmiXLXB1AGLm8KBjfE8s3L5xqi+yUod+j8MtvIj812dkS4QMiRVN/by2h3ZY8LYVGrqZXZTcgn2ujn8uKjXLZVD5TdQIDAQABAoIBAHREk0I0O9DvECKdWUpAmF3mY7oY9PNQiu44Yaf+AoSuyRpRUGTMIgc3u3eivOE8ALX0BmYUO5JtuRNZDpvt4SAwqCnVUinIf6C+eH/wSurCpapSM0BAHp4aOA7igptyOMgMPYBHNA1e9A7jE0dCxKWMl3DSWNyjQTk4zeRGEAEfbNjHrq6YCtjHSZSLmWiG80hnfnYos9hOr5JnLnyS7ZmFE/5P3XVrxLc/tQ5zum0R4cbrgzHiQP5RgfxGJaEi7XcgherCCOgurJSSbYH29Gz8u5fFbS+Yg8s+OiCss3cs1rSgJ9/eHZuzGEdUZVARH6hVMjSuwvqVTFaE8AgtleECgYEA+uLMn4kNqHlJS2A5uAnCkj90ZxEtNm3E8hAxUrhssktY5XSOAPBlxyf5RuRGIImGtUVIr4HuJSa5TX48n3Vdt9MYCprO/iYl6moNRSPt5qowIIOJmIjY2mqPDfDt/zw+fcDD3lmCJrFlzcnh0uea1CohxEbQnL3cypeLt+WbU6kCgYEAzSp19m1ajieFkqgoB0YTpt/OroDx38vvI5unInJlEeOjQ+oIAQdN2wpxBvTrRorMU6P07mFUbt1j+Co6CbNiw+X8HcCaqYLR5clbJOOWNR36PuzOpQLkfK8woupBxzW9B8gZmY8rB1mbJ+/WTPrEJy6YGmIEBkWylQ2VpW8O4O0CgYEApdbvvfFBlwD9YxbrcGz7MeNCFbMz+MucqQntIKoKJ91ImPxvtc0y6e/Rhnv0oyNlaUOwJVu0yNgNG117w0g4t/+Q38mvVC5xV7/cn7x9UMFk6MkqVir3dYGEqIl/OP1grY2Tq9HtB5iyG9L8NIamQOLMyUqqMUILxdthHyFmiGkCgYEAn9+PjpjGMPHxL0gj8Q8VbzsFtou6b1deIRRA2CHmSltltR1gYVTMwXxQeUhPMmgkMqUXzs4/WijgpthY44hK1TaZEKIuoxrS70nJ4WQLf5a9k1065fDsFZD6yGjdGxvwEmlGMZgTwqV7t1I4X0Ilqhav5hcs5apYL7gnPYPeRz0CgYALHCj/Ji8XSsDoF/MhVhnGdIs2P99NNdmo3R2Pv0CuZbDKMU559LJHUvrKS8WkuWRDuKrz1W/EQKApFjDGpdqToZqriUFQzwy7mR3ayIiogzNtHcvbDHx8oFnGY0OFksX/ye0/XGpy2SFxYRwGU98HPYeBvAQQrVjdkzfy7BmXQQ==";

    #[derive(Serialize)]
    struct Claims<'a> {
        iss: &'a str,
        aud: Value,
        sub: &'a str,
        exp: i64,
        iat: i64,
        nbf: i64,
        jti: &'a str,
        repository_id: &'a str,
    }

    fn binding(issuer: &str) -> PublisherBinding {
        PublisherBinding {
            id: "github-release".to_owned(),
            repository: "private".to_owned(),
            publisher: TrustedPublisher {
                issuer: issuer.to_owned(),
                audience: "peryx".to_owned(),
                subject: Glob::new("repo:org/app:*"),
                claims: BTreeMap::from([("repository_id".to_owned(), "42".to_owned())]),
                projects: vec![Glob::new("app")],
            },
        }
    }

    fn test_runtime(issuer: &str) -> OidcRuntime {
        OidcRuntime::new_insecure(vec![binding(issuer)], Signer::new(b"local-key", "peryx"), 300).unwrap()
    }

    fn test_runtime_with_replay_capacity(issuer: &str, capacity: usize) -> OidcRuntime {
        OidcRuntime::new_insecure_with_replay_capacity(
            vec![binding(issuer)],
            Signer::new(b"local-key", "peryx"),
            300,
            capacity,
        )
        .unwrap()
    }

    async fn mount_issuer_with(server: &MockServer, keys: Value, content_type: &str, cache_control: Option<&str>) {
        let mut discovery = ResponseTemplate::new(200).set_body_raw(
            json!({
                "issuer": server.uri(),
                "jwks_uri": format!("{}/keys", server.uri()),
                "id_token_signing_alg_values_supported": ["RS256"]
            })
            .to_string(),
            content_type,
        );
        let mut jwks = ResponseTemplate::new(200)
            .insert_header("content-type", "application/json")
            .set_body_json(keys);
        if let Some(cache_control) = cache_control {
            discovery = discovery.insert_header("cache-control", cache_control);
            jwks = jwks.insert_header("cache-control", cache_control);
        }
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(discovery)
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/keys"))
            .respond_with(jwks)
            .mount(server)
            .await;
    }

    async fn mount_issuer(server: &MockServer, keys: Value) {
        mount_issuer_with(server, keys, "application/json", Some("max-age=120")).await;
    }

    fn jwk(kid: &str) -> Value {
        json!({"kty": "RSA", "n": MODULUS, "e": "AQAB", "kid": kid, "alg": "RS256", "use": "sig"})
    }

    fn encoding_key() -> EncodingKey {
        use base64::Engine as _;
        EncodingKey::from_rsa_der(
            &base64::engine::general_purpose::STANDARD
                .decode(PRIVATE_KEY_DER)
                .unwrap(),
        )
    }

    fn identity_with_expiry(issuer: &str, kid: &str, jti: &str, expires_at: i64) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some(kid.to_owned());
        jsonwebtoken::encode(
            &header,
            &Claims {
                iss: issuer,
                aud: Value::String("peryx".to_owned()),
                sub: "repo:org/app:ref:refs/heads/main",
                exp: expires_at,
                iat: NOW,
                nbf: NOW,
                jti,
                repository_id: "42",
            },
            &encoding_key(),
        )
        .unwrap()
    }

    fn identity(issuer: &str, kid: &str, jti: &str) -> String {
        identity_with_expiry(issuer, kid, jti, NOW + 600)
    }

    async fn runtime() -> (MockServer, Arc<OidcRuntime>) {
        let server = MockServer::start().await;
        mount_issuer(&server, json!({"keys": [jwk("key-1")]})).await;
        let runtime = Arc::new(test_runtime(&server.uri()));
        (server, runtime)
    }

    #[tokio::test]
    async fn test_exchange_mints_a_route_scoped_token_once() {
        let (server, runtime) = runtime().await;
        let external = identity(&server.uri(), "key-1", "external-1");
        let exchanged = runtime.exchange(&external, NOW).await.unwrap();
        let internal = Signer::new(b"local-key", "peryx")
            .verify_trusted(&exchanged.token)
            .unwrap();
        assert_eq!(exchanged.publisher_id, "github-release");
        assert_eq!(internal.id, exchanged.token_id);
        assert!(crate::authorize_grants(&internal.grants, Some("private/app"), crate::Action::Write).is_ok());
        assert!(crate::authorize_grants(&internal.grants, Some("other/app"), crate::Action::Write).is_err());
        assert!(matches!(
            runtime.exchange(&external, NOW).await,
            Err(ExchangeError::Replay)
        ));
    }

    #[tokio::test]
    async fn test_concurrent_exchange_has_one_winner() {
        let (server, runtime) = runtime().await;
        let token = identity(&server.uri(), "key-1", "race");
        let (first, second) = tokio::join!(runtime.exchange(&token, NOW), runtime.exchange(&token, NOW));
        assert_eq!(
            (
                usize::from(first.is_ok()) + usize::from(second.is_ok()),
                usize::from(matches!(first, Err(ExchangeError::Replay)))
                    + usize::from(matches!(second, Err(ExchangeError::Replay))),
            ),
            (1, 1)
        );
    }

    #[tokio::test]
    async fn test_duplicate_key_ids_reject_the_refresh() {
        let server = MockServer::start().await;
        mount_issuer(&server, json!({"keys": [jwk("same"), jwk("same")]})).await;
        let runtime = test_runtime(&server.uri());
        assert!(matches!(
            runtime.exchange(&identity(&server.uri(), "same", "jti"), NOW).await,
            Err(ExchangeError::InvalidIssuerResponse)
        ));
    }

    #[tokio::test]
    async fn test_missing_key_id_rejects_the_refresh() {
        let server = MockServer::start().await;
        mount_issuer(
            &server,
            json!({"keys": [{"kty": "RSA", "n": MODULUS, "e": "AQAB", "alg": "RS256"}]}),
        )
        .await;
        let runtime = test_runtime(&server.uri());
        assert!(matches!(
            runtime.exchange(&identity(&server.uri(), "key-1", "jti"), NOW).await,
            Err(ExchangeError::InvalidIssuerResponse)
        ));
    }

    #[tokio::test]
    async fn test_incompatible_keys_do_not_hide_a_usable_key() {
        let server = MockServer::start().await;
        mount_issuer(
            &server,
            json!({"keys": [
                {"kty": "oct", "k": "c2VjcmV0", "kid": "symmetric", "alg": "HS256"},
                {"kty": "RSA", "n": MODULUS, "e": "AQAB", "kid": "signing-only", "alg": "RS256", "use": "sig", "key_ops": ["sign"]},
                {"kty": "RSA", "n": MODULUS, "e": "AQAB", "kid": "key-1", "alg": "RS256", "use": "sig", "key_ops": ["verify"]}
            ]}),
        )
        .await;
        let runtime = test_runtime(&server.uri());
        assert!(
            runtime
                .exchange(&identity(&server.uri(), "key-1", "mixed"), NOW)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn test_key_set_without_a_usable_key_is_rejected() {
        let server = MockServer::start().await;
        mount_issuer(
            &server,
            json!({"keys": [
                {"kty": "oct", "k": "c2VjcmV0", "kid": "symmetric", "alg": "HS256"}
            ]}),
        )
        .await;
        let runtime = test_runtime(&server.uri());
        assert!(matches!(
            runtime.exchange(&identity(&server.uri(), "key-1", "jti"), NOW).await,
            Err(ExchangeError::InvalidIssuerResponse)
        ));
    }

    #[tokio::test]
    async fn test_malformed_refresh_keeps_the_working_key() {
        let (server, runtime) = runtime().await;
        runtime
            .exchange(&identity(&server.uri(), "key-1", "warm"), NOW)
            .await
            .unwrap();
        server.reset().await;
        mount_issuer(
            &server,
            json!({"keys": [{"kty": "RSA", "n": "!", "e": "AQAB", "kid": "key-1", "alg": "RS256"}]}),
        )
        .await;
        assert!(
            runtime
                .exchange(&identity(&server.uri(), "key-1", "stale"), NOW + 121)
                .await
                .is_ok()
        );
        assert!(
            runtime
                .exchange(&identity(&server.uri(), "key-1", "cached"), NOW + 122)
                .await
                .is_ok()
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
        assert!(matches!(
            runtime
                .exchange(&identity(&server.uri(), "key-1", "expired"), NOW + HARD_CACHE_SECS + 1)
                .await,
            Err(ExchangeError::InvalidIssuerResponse)
        ));
        assert_eq!(server.received_requests().await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn test_replay_capacity_recovers_after_identity_expiry() {
        let (server, _runtime) = runtime().await;
        let runtime = test_runtime_with_replay_capacity(&server.uri(), 1);
        runtime
            .exchange(&identity_with_expiry(&server.uri(), "key-1", "first", NOW + 1), NOW)
            .await
            .unwrap();
        assert!(matches!(
            runtime.exchange(&identity(&server.uri(), "key-1", "full"), NOW).await,
            Err(ExchangeError::ReplayCapacity)
        ));
        assert!(
            runtime
                .exchange(&identity(&server.uri(), "key-1", "recovered"), NOW + 2)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn test_bad_signature_does_not_refresh_a_warm_key() {
        let (server, runtime) = runtime().await;
        runtime
            .exchange(&identity(&server.uri(), "key-1", "warm"), NOW)
            .await
            .unwrap();
        let mut bad = identity(&server.uri(), "key-1", "bad");
        bad.push('x');
        assert!(matches!(
            runtime.exchange(&bad, NOW).await,
            Err(ExchangeError::InvalidIdentity)
        ));
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn test_cold_issuer_failure_is_rate_limited() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let runtime = test_runtime(&server.uri());
        let token = identity(&server.uri(), "key-1", "cold");
        assert!(matches!(runtime.exchange(&token, NOW).await, Err(error) if error.unavailable()));
        assert!(matches!(runtime.exchange(&token, NOW).await, Err(error) if error.unavailable()));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_unknown_key_refresh_is_single_flight() {
        let (server, runtime) = runtime().await;
        runtime
            .exchange(&identity(&server.uri(), "key-1", "warm"), NOW)
            .await
            .unwrap();
        let unknown = identity(&server.uri(), "key-2", "unknown");
        let (first, second) = tokio::join!(
            runtime.exchange(&unknown, NOW + 61),
            runtime.exchange(&unknown, NOW + 61)
        );
        assert!(matches!(first, Err(ExchangeError::UnknownKey)));
        assert!(matches!(second, Err(ExchangeError::UnknownKey)));
        assert_eq!(server.received_requests().await.unwrap().len(), 4);
    }

    #[tokio::test]
    async fn test_claim_time_and_shape_failures_are_closed() {
        let (server, runtime) = runtime().await;
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("key-1".to_owned());
        let key = encoding_key();
        for (jti, claims) in [
            (
                "multi-aud",
                Claims {
                    iss: &server.uri(),
                    aud: json!(["peryx", "other"]),
                    sub: "repo:org/app:x",
                    exp: NOW + 10,
                    iat: NOW,
                    nbf: NOW,
                    jti: "multi-aud",
                    repository_id: "42",
                },
            ),
            (
                "future",
                Claims {
                    iss: &server.uri(),
                    aud: json!("peryx"),
                    sub: "repo:org/app:x",
                    exp: NOW + 20,
                    iat: NOW + 10,
                    nbf: NOW + 10,
                    jti: "future",
                    repository_id: "42",
                },
            ),
            (
                "long",
                Claims {
                    iss: &server.uri(),
                    aud: json!("peryx"),
                    sub: "repo:org/app:x",
                    exp: NOW + MAX_IDENTITY_LIFETIME_SECS + 1,
                    iat: NOW,
                    nbf: NOW,
                    jti: "long",
                    repository_id: "42",
                },
            ),
            (
                "overflow",
                Claims {
                    iss: &server.uri(),
                    aud: json!("peryx"),
                    sub: "repo:org/app:x",
                    exp: i64::MAX,
                    iat: i64::MIN,
                    nbf: i64::MIN,
                    jti: "overflow",
                    repository_id: "42",
                },
            ),
        ] {
            let token = jsonwebtoken::encode(&header, &claims, &key).unwrap();
            assert!(
                matches!(runtime.exchange(&token, NOW).await, Err(ExchangeError::InvalidIdentity)),
                "{jti}"
            );
        }
    }

    #[tokio::test]
    async fn test_claim_text_bounds_are_closed() {
        let (server, runtime) = runtime().await;
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("key-1".to_owned());
        for (name, sub, jti) in [
            ("subject", "s".repeat(MAX_SUBJECT_BYTES + 1), "jti".to_owned()),
            ("jti", "subject".to_owned(), "j".repeat(MAX_JTI_BYTES + 1)),
        ] {
            let token = jsonwebtoken::encode(
                &header,
                &Claims {
                    iss: &server.uri(),
                    aud: json!("peryx"),
                    sub: &sub,
                    exp: NOW + 60,
                    iat: NOW,
                    nbf: NOW,
                    jti: &jti,
                    repository_id: "42",
                },
                &encoding_key(),
            )
            .unwrap();
            assert!(
                matches!(runtime.exchange(&token, NOW).await, Err(ExchangeError::InvalidIdentity)),
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn test_token_size_and_algorithm_are_closed() {
        let (server, runtime) = runtime().await;
        assert!(matches!(
            runtime.exchange(&"x".repeat(TOKEN_BODY_LIMIT + 1), NOW).await,
            Err(ExchangeError::InvalidIdentity)
        ));
        let token = jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &Claims {
                iss: &server.uri(),
                aud: json!("peryx"),
                sub: "repo:org/app:x",
                exp: NOW + 60,
                iat: NOW,
                nbf: NOW,
                jti: "wrong-algorithm",
                repository_id: "42",
            },
            &EncodingKey::from_secret(b"secret"),
        )
        .unwrap();
        assert!(matches!(
            runtime.exchange(&token, NOW).await,
            Err(ExchangeError::InvalidIdentity)
        ));
    }

    #[tokio::test]
    async fn test_discovery_must_repeat_the_issuer_and_algorithm() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_json(json!({
                        "issuer": "https://other.example",
                        "jwks_uri": format!("{}/keys", server.uri()),
                        "id_token_signing_alg_values_supported": ["ES256"]
                    })),
            )
            .mount(&server)
            .await;
        let runtime = test_runtime(&server.uri());
        assert!(matches!(
            runtime.exchange(&identity(&server.uri(), "key-1", "jti"), NOW).await,
            Err(ExchangeError::InvalidIssuerResponse)
        ));
    }

    #[tokio::test]
    async fn test_chunked_issuer_body_is_bounded_while_streaming() {
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
        let issuer = format!("http://{address}");
        let runtime = test_runtime(&issuer);
        assert!(matches!(
            runtime.exchange(&identity(&issuer, "key-1", "large"), NOW).await,
            Err(ExchangeError::InvalidIssuerResponse)
        ));
    }

    #[tokio::test]
    async fn test_truncated_issuer_body_is_unavailable() {
        use std::io::{Read as _, Write as _};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0; 1024];
            let _ = socket.read(&mut request);
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 8\r\nconnection: close\r\n\r\n{}",
                )
                .unwrap();
        });
        let issuer = format!("http://{address}");
        let runtime = test_runtime(&issuer);
        assert!(matches!(
            runtime.exchange(&identity(&issuer, "key-1", "truncated"), NOW).await,
            Err(ExchangeError::IssuerUnavailable)
        ));
    }

    #[test]
    fn test_runtime_rejects_an_empty_publisher_set() {
        assert!(matches!(
            OidcRuntime::new(Vec::new(), Signer::new(b"key", "peryx"), 60),
            Err(ExchangeError::Configuration)
        ));
    }

    #[test]
    fn test_runtime_rejects_a_nonpositive_token_lifetime() {
        assert!(matches!(
            OidcRuntime::new(vec![binding("https://issuer.example")], Signer::new(b"key", "peryx"), 0,),
            Err(ExchangeError::Configuration)
        ));
    }

    #[test]
    fn test_runtime_rejects_an_empty_publisher_id() {
        let mut invalid_id = binding("https://issuer.example");
        invalid_id.id.clear();
        assert!(matches!(
            OidcRuntime::new(vec![invalid_id], Signer::new(b"key", "peryx"), 60),
            Err(ExchangeError::Configuration)
        ));
    }

    #[rstest]
    #[case::malformed("not a URL")]
    #[case::http("http://id.example")]
    #[case::credentials("https://user@id.example")]
    fn test_runtime_rejects_an_invalid_issuer(#[case] issuer: &str) {
        assert!(matches!(
            OidcRuntime::new(vec![binding(issuer)], Signer::new(b"key", "peryx"), 60),
            Err(ExchangeError::Configuration)
        ));
    }

    #[test]
    fn test_production_runtime_accepts_an_https_issuer() {
        let runtime = OidcRuntime::new(
            vec![binding("https://issuer.example")],
            Signer::new(b"key", "peryx"),
            60,
        )
        .unwrap();
        assert_eq!(runtime.audience(), "peryx");
    }

    #[tokio::test]
    async fn test_discovery_uses_the_configured_issuer_path() {
        let server = MockServer::start().await;
        let issuer = format!("{}/tenant/", server.uri());
        Mock::given(method("GET"))
            .and(path("/tenant/.well-known/openid-configuration"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_json(json!({
                        "issuer": &issuer,
                        "jwks_uri": format!("{}/keys", server.uri()),
                        "id_token_signing_alg_values_supported": ["RS256"]
                    })),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/keys"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_json(json!({"keys": [jwk("key-1")]})),
            )
            .mount(&server)
            .await;
        let runtime = test_runtime(&issuer);
        assert!(runtime.exchange(&identity(&issuer, "key-1", "path"), NOW).await.is_ok());
    }

    #[rstest]
    #[case::quoted_zero(Some("private, max-age=\"0\", must-revalidate"), 60)]
    #[case::absent(None, 300)]
    #[tokio::test]
    async fn test_cache_control_sets_refresh_time(#[case] cache_control: Option<&str>, #[case] fresh_for: i64) {
        let server = MockServer::start().await;
        mount_issuer_with(
            &server,
            json!({"keys": [jwk("key-1")]}),
            "application/json",
            cache_control,
        )
        .await;
        let runtime = test_runtime(&server.uri());
        runtime
            .exchange(&identity(&server.uri(), "key-1", "cold"), NOW)
            .await
            .unwrap();
        runtime
            .exchange(&identity(&server.uri(), "key-1", "fresh"), NOW + fresh_for - 1)
            .await
            .unwrap();
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
        runtime
            .exchange(&identity(&server.uri(), "key-1", "refresh"), NOW + fresh_for)
            .await
            .unwrap();
        assert_eq!(server.received_requests().await.unwrap().len(), 4);
    }

    #[rstest]
    #[case::json("application/json; charset=utf-8", true)]
    #[case::structured("application/jwk-set+json", true)]
    #[case::case_insensitive("Application/JSON", true)]
    #[case::wrong_type("text/json", false)]
    #[tokio::test]
    async fn test_discovery_content_type_is_enforced(#[case] content_type: &str, #[case] accepted: bool) {
        let server = MockServer::start().await;
        mount_issuer_with(
            &server,
            json!({"keys": [jwk("key-1")]}),
            content_type,
            Some("max-age=120"),
        )
        .await;
        let result = test_runtime(&server.uri())
            .exchange(&identity(&server.uri(), "key-1", "content-type"), NOW)
            .await;
        if accepted {
            assert!(result.is_ok());
        } else {
            assert!(matches!(result, Err(ExchangeError::InvalidIssuerResponse)));
        }
    }

    #[tokio::test]
    async fn test_empty_repository_keeps_project_grants_unqualified() {
        let server = MockServer::start().await;
        mount_issuer(&server, json!({"keys": [jwk("key-1")]})).await;
        let mut binding = binding(&server.uri());
        binding.repository.clear();
        let signer = Signer::new(b"local-key", "peryx");
        let runtime = OidcRuntime::new_insecure(vec![binding], signer.clone(), 300).unwrap();
        let exchanged = runtime
            .exchange(&identity(&server.uri(), "key-1", "unqualified"), NOW)
            .await
            .unwrap();
        assert_eq!(
            signer.verify_trusted(&exchanged.token).unwrap().grants,
            vec![Grant {
                projects: vec![Glob::new("app")],
                actions: std::collections::BTreeSet::from([crate::Action::Write]),
            }]
        );
    }

    #[rstest]
    #[case::issuer_unavailable(ExchangeError::IssuerUnavailable, true)]
    #[case::invalid_response(ExchangeError::InvalidIssuerResponse, true)]
    #[case::unknown_key(ExchangeError::UnknownKey, true)]
    #[case::replay_capacity(ExchangeError::ReplayCapacity, true)]
    #[case::configuration(ExchangeError::Configuration, false)]
    #[case::invalid_identity(ExchangeError::InvalidIdentity, false)]
    #[case::replay(ExchangeError::Replay, false)]
    #[case::denied(ExchangeError::Denied(PublishDenial::UnknownIssuer), false)]
    fn test_exchange_error_availability(#[case] error: ExchangeError, #[case] expected: bool) {
        assert_eq!(error.unavailable(), expected);
    }
}
