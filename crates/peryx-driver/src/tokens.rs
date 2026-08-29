//! Token secrets are returned once. Storage and lifecycle events contain only verifiers and metadata.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

use peryx_events::security::Event;
use peryx_identity::{Action, GrantScope, TokenId, TokenName, TokenSecret, TokenVerifier, UserId};
use peryx_storage::meta::{
    MetaError, MetaStore, NewScopedToken, RevokeScopedTokenOutcome, ScopedTokenPage, ScopedTokenWriteError,
};
pub use peryx_storage::meta::{ScopedTokenQuery, ScopedTokenQueryError, ScopedTokenRecord};

/// A second verifier collision signals a broken secret source, so stop instead of looping.
const SECRET_GENERATION_ATTEMPTS: usize = 2;

/// Persistent scoped-token operations over the metadata store.
#[derive(Clone)]
pub struct TokenService {
    store: MetaStore,
    secret_source: Arc<dyn Fn() -> TokenSecret + Send + Sync>,
}

/// A token to mint: the reach and actions to grant, validated against the caller's authority before it
/// reaches this service.
#[derive(Debug, Clone)]
pub struct CreateScopedToken {
    pub name: TokenName,
    pub reach: GrantScope,
    pub actions: BTreeSet<Action>,
    pub expires_at: Option<i64>,
    pub created_by: UserId,
}

#[derive(Debug, thiserror::Error)]
pub enum TokenServiceError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("scoped-token secret generation exhausted after verifier collisions")]
    SecretGenerationExhausted,
}

impl TokenService {
    #[must_use]
    pub fn new(store: MetaStore) -> Self {
        Self::with_secret_source(store, TokenSecret::generate)
    }

    /// The source must return independent 256-bit secrets from a CSPRNG.
    #[must_use]
    pub fn with_secret_source(
        store: MetaStore,
        secret_source: impl Fn() -> TokenSecret + Send + Sync + 'static,
    ) -> Self {
        Self {
            store,
            secret_source: Arc::new(secret_source),
        }
    }

    /// Mint a token, returning its record and the one-time secret a client must store now.
    ///
    /// # Errors
    /// Returns an exhaustion error when the secret source repeats owned verifiers, or a store error when
    /// the token cannot be persisted.
    pub fn create(
        &self,
        request: CreateScopedToken,
        now: i64,
    ) -> Result<(ScopedTokenRecord, TokenSecret), TokenServiceError> {
        let CreateScopedToken {
            name,
            reach,
            actions,
            expires_at,
            created_by,
        } = request;
        let (record, secret) = self.with_generated_secret(|verifier| {
            self.store.create_scoped_token(NewScopedToken {
                name: name.clone(),
                reach: reach.clone(),
                actions: actions.clone(),
                expires_at,
                verifier: verifier.clone(),
                created_by: created_by.clone(),
                created_at_unix: now,
            })
        })?;
        emit("scoped_token_created", &record.created_by, &record.id);
        Ok((record, secret))
    }

    /// # Errors
    /// Returns a store error when the row cannot be read.
    pub fn inspect(&self, id: &TokenId) -> Result<Option<ScopedTokenRecord>, MetaError> {
        self.store.get_scoped_token(id)
    }

    /// # Errors
    /// Returns a query or store error.
    pub fn list(&self, query: &ScopedTokenQuery) -> Result<ScopedTokenPage, ScopedTokenQueryError> {
        self.store.list_scoped_tokens(query)
    }

    /// # Errors
    /// Returns an exhaustion error when the secret source repeats owned verifiers, or a store error when
    /// the rotation cannot be committed.
    pub fn rotate(
        &self,
        id: &TokenId,
        actor: &UserId,
    ) -> Result<Option<(ScopedTokenRecord, TokenSecret)>, TokenServiceError> {
        let (record, secret) = self.with_generated_secret(|verifier| self.store.rotate_scoped_token(id, verifier))?;
        let Some(record) = record else {
            return Ok(None);
        };
        emit("scoped_token_rotated", actor, &record.id);
        Ok(Some((record, secret)))
    }

    /// # Errors
    /// Returns a store error when the revocation cannot be committed.
    pub fn revoke(
        &self,
        id: &TokenId,
        actor: &UserId,
        now: i64,
    ) -> Result<Option<RevokeScopedTokenOutcome>, MetaError> {
        let outcome = self.store.revoke_scoped_token(id, now)?;
        if let Some(RevokeScopedTokenOutcome::Revoked(record)) = &outcome {
            emit("scoped_token_revoked", actor, &record.id);
        }
        Ok(outcome)
    }

    /// # Errors
    /// Returns a store error when the lookup cannot be read.
    pub fn verify(&self, presented: &TokenSecret, now: i64) -> Result<Option<ScopedTokenRecord>, MetaError> {
        self.store.verify_scoped_token(presented, now)
    }

    fn with_generated_secret<T>(
        &self,
        mut persist: impl FnMut(&TokenVerifier) -> Result<T, ScopedTokenWriteError>,
    ) -> Result<(T, TokenSecret), TokenServiceError> {
        for _ in 0..SECRET_GENERATION_ATTEMPTS {
            let secret = (self.secret_source)();
            match persist(&secret.verifier()) {
                Ok(value) => return Ok((value, secret)),
                Err(ScopedTokenWriteError::VerifierCollision) => {}
                Err(ScopedTokenWriteError::Store(error)) => return Err(error.into()),
            }
        }
        Err(TokenServiceError::SecretGenerationExhausted)
    }
}

impl fmt::Debug for TokenService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TokenService")
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

fn emit(action: &'static str, actor: &UserId, id: &TokenId) {
    Event::new(action, "success")
        .actor(Some(actor.as_str()))
        .token_id(id.as_str())
        .emit();
}
