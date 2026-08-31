use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use peryx_driver::oidc::GuardedOidcTransport;
use peryx_identity::{
    Glob, Grant, OidcTokenVerifier, OidcVerificationError, OidcVerifier, Principal, Signer, TokenScope, VerifiedToken,
};

use super::policy::{PublishClaims, PublishDenial, TrustedPublisher, authorize_publish};

pub(super) const TOKEN_SCOPE: TokenScope = TokenScope::new("trusted-publishing");
const MAX_REPLAY_ENTRIES: usize = 65_536;
const VERIFIER_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PublisherBinding {
    pub id: String,
    pub repository: String,
    pub route: String,
    pub publisher: TrustedPublisher,
}

#[async_trait]
pub(super) trait IdentityExchange: Send + Sync {
    fn audience(&self) -> &str;

    async fn exchange(&self, token: &str, now: i64) -> Result<ExchangedToken, ExchangeError>;
}

pub struct OidcRuntime {
    audience: String,
    bindings: Vec<PublisherBinding>,
    publishers: Vec<TrustedPublisher>,
    verifier: Arc<dyn OidcTokenVerifier>,
    signer: Signer,
    token_ttl_secs: i64,
    replay: Mutex<HashMap<(String, String), i64>>,
    replay_capacity: usize,
}

impl OidcRuntime {
    pub(super) fn new(
        bindings: Vec<PublisherBinding>,
        trusted_endpoint_hosts: &[String],
        signer: Signer,
        token_ttl_secs: i64,
    ) -> Result<Self, ExchangeError> {
        let first = bindings.first().ok_or(ExchangeError::Configuration)?;
        let transport = GuardedOidcTransport::new(
            bindings.iter().map(|binding| binding.publisher.issuer.as_str()),
            trusted_endpoint_hosts,
            VERIFIER_REQUEST_TIMEOUT,
        )
        .map_err(|_| ExchangeError::Configuration)?;
        let verifier = OidcVerifier::new(
            bindings.iter().map(|binding| binding.publisher.issuer.clone()),
            first.publisher.audience.clone(),
            Arc::new(transport),
        )
        .map_err(|_| ExchangeError::Configuration)?;
        Self::build(bindings, Arc::new(verifier), signer, token_ttl_secs, MAX_REPLAY_ENTRIES)
    }

    fn build(
        bindings: Vec<PublisherBinding>,
        verifier: Arc<dyn OidcTokenVerifier>,
        signer: Signer,
        token_ttl_secs: i64,
        replay_capacity: usize,
    ) -> Result<Self, ExchangeError> {
        if token_ttl_secs <= 0 || replay_capacity == 0 {
            return Err(ExchangeError::Configuration);
        }
        let first = bindings.first().ok_or(ExchangeError::Configuration)?;
        let audience = first.publisher.audience.clone();
        let mut ids = std::collections::HashSet::new();
        if bindings.iter().any(|binding| {
            binding.id.trim().is_empty()
                || binding.route.contains("..")
                || binding.publisher.audience != audience
                || !ids.insert(binding.id.clone())
        }) {
            return Err(ExchangeError::Configuration);
        }
        Ok(Self {
            audience,
            publishers: bindings.iter().map(|binding| binding.publisher.clone()).collect(),
            bindings,
            verifier,
            signer,
            token_ttl_secs,
            replay: Mutex::new(HashMap::new()),
            replay_capacity,
        })
    }

    pub(crate) fn verify_upload(&self, token: &str) -> Result<VerifiedToken, peryx_identity::TokenError> {
        self.signer.verify_scoped(token, TOKEN_SCOPE)
    }

    async fn exchange_token(&self, token: &str, now: i64) -> Result<ExchangedToken, ExchangeError> {
        let verified = self
            .verifier
            .verify(token, &self.audience, now)
            .await
            .map_err(ExchangeError::Verification)?;
        let claims = PublishClaims {
            issuer: verified.issuer,
            audience: verified.audience,
            subject: verified.subject,
            expires_at: verified.expires_at,
            claims: verified
                .claims
                .into_iter()
                .filter_map(|(name, value)| value.as_str().map(|value| (name, value.to_owned())))
                .collect::<BTreeMap<_, _>>(),
        };
        let (position, mut grants) = authorize_publish(&self.publishers, &claims, now)?;
        let binding = &self.bindings[position];
        qualify_grants(&mut grants, &binding.route);
        let ttl_secs = self.token_ttl_secs.min(claims.expires_at - now);
        let token_id = uuid::Uuid::new_v4().to_string();
        let principal = Principal::Named {
            subject: format!("trusted-publisher:{}", binding.id),
        };
        let token = self
            .signer
            .mint_scoped(TOKEN_SCOPE, &principal, &grants, now, ttl_secs, &token_id);
        self.consume_replay(&claims.issuer, &verified.token_id, claims.expires_at, now)?;
        Ok(ExchangedToken {
            token,
            token_id,
            publisher_id: binding.id.clone(),
            repository: binding.repository.clone(),
            expires_at: now + ttl_secs,
        })
    }

    fn consume_replay(&self, issuer: &str, token_id: &str, expires_at: i64, now: i64) -> Result<(), ExchangeError> {
        let mut replay = self.replay.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        replay.retain(|_, expiry| *expiry > now);
        let key = (issuer.to_owned(), token_id.to_owned());
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
        &self.audience
    }

    async fn exchange(&self, token: &str, now: i64) -> Result<ExchangedToken, ExchangeError> {
        self.exchange_token(token, now).await
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct ExchangedToken {
    pub token: String,
    pub token_id: String,
    pub publisher_id: String,
    pub repository: String,
    pub expires_at: i64,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum ExchangeError {
    #[error("trusted publishing is misconfigured")]
    Configuration,
    #[error("the identity token has already been exchanged")]
    Replay,
    #[error("the identity replay cache is full")]
    ReplayCapacity,
    #[error(transparent)]
    Verification(OidcVerificationError),
    #[error(transparent)]
    Denied(#[from] PublishDenial),
}

impl ExchangeError {
    pub(super) const fn unavailable(&self) -> bool {
        matches!(self, Self::ReplayCapacity) || matches!(self, Self::Verification(error) if error.unavailable())
    }
}

fn qualify_grants(grants: &mut [Grant], repository: &str) {
    if repository.is_empty() {
        return;
    }
    for grant in grants {
        for resource in &mut grant.resources {
            *resource = Glob::new(format!("{repository}/{}", resource.as_str()));
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/trusted_publishing/runtime_tests.rs"]
mod tests;
