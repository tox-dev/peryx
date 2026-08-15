//! Token secrets are returned once. Storage and lifecycle events contain only verifiers and metadata.

use std::collections::BTreeSet;

use peryx_events::security::Event;
use peryx_identity::{Action, GrantScope, TokenId, TokenName, TokenSecret, UserId};
use peryx_storage::meta::{MetaError, MetaStore, NewScopedToken, RevokeScopedTokenOutcome, ScopedTokenPage};
pub use peryx_storage::meta::{ScopedTokenQuery, ScopedTokenQueryError, ScopedTokenRecord};

/// Persistent scoped-token operations over the metadata store.
#[derive(Debug, Clone)]
pub struct TokenService {
    store: MetaStore,
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

impl TokenService {
    #[must_use]
    pub const fn new(store: MetaStore) -> Self {
        Self { store }
    }

    /// Mint a token, returning its record and the one-time secret a client must store now.
    ///
    /// # Errors
    /// Returns a store error when the token cannot be persisted.
    pub fn create(&self, request: CreateScopedToken, now: i64) -> Result<(ScopedTokenRecord, TokenSecret), MetaError> {
        let secret = TokenSecret::generate();
        let record = self.store.create_scoped_token(NewScopedToken {
            name: request.name,
            reach: request.reach,
            actions: request.actions,
            expires_at: request.expires_at,
            verifier: secret.verifier(),
            created_by: request.created_by,
            created_at_unix: now,
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
    /// Returns a store error when the rotation cannot be committed.
    pub fn rotate(&self, id: &TokenId, actor: &UserId) -> Result<Option<(ScopedTokenRecord, TokenSecret)>, MetaError> {
        let secret = TokenSecret::generate();
        let Some(record) = self.store.rotate_scoped_token(id, &secret.verifier())? else {
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
}

fn emit(action: &'static str, actor: &UserId, id: &TokenId) {
    Event::new(action, "success")
        .actor(Some(actor.as_str()))
        .token_id(id.as_str())
        .emit();
}
