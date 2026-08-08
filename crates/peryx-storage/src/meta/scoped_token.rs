//! Persisted scoped API tokens: the authorized create/list/inspect/rotate/revoke lifecycle behind the
//! management API, keyed for a one-read verification that writes nothing.
//!
//! A token's plaintext secret is never stored; the row keeps only its [`TokenVerifier`] digest, indexed
//! so a presented secret resolves its token with a single lookup. Revocation removes that index entry,
//! so a revoked token stops authenticating on the very next request while its record and lifecycle
//! evidence remain for an administrator to inspect.

use std::collections::BTreeSet;
use std::ops::Bound::{Excluded, Included, Unbounded};

use peryx_identity::{Action, GrantScope, TokenId, TokenName, TokenSecret, TokenVerifier, UserId};
use redb::{ReadableTable as _, WriteTransaction};
use serde::{Deserialize, Serialize};

use super::{MetaError, MetaStore, SCOPED_TOKEN, SCOPED_TOKEN_REACH, SCOPED_TOKEN_VERIFIER};

const MAX_QUERY_LIMIT: usize = 100;

/// The metadata of one scoped token, everything the management API returns. It never carries the secret
/// or its verifier, so serializing it to a client discloses nothing usable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopedTokenRecord {
    pub id: TokenId,
    pub name: TokenName,
    /// The reach a token authenticates over: the whole server, or one named repository.
    pub reach: GrantScope,
    pub actions: BTreeSet<Action>,
    pub created_by: UserId,
    pub created_at_unix: i64,
    /// Unix seconds after which the token stops authenticating; `None` never expires.
    pub expires_at: Option<i64>,
    /// Unix seconds the token was revoked; `None` while it is live.
    pub revoked_at: Option<i64>,
    pub revision: u64,
}

impl ScopedTokenRecord {
    /// Whether the token may authenticate at `now`: not revoked, and not past its expiry.
    #[must_use]
    pub fn is_live(&self, now: i64) -> bool {
        self.revoked_at.is_none() && self.expires_at.is_none_or(|expiry| now < expiry)
    }
}

/// The stored row: the client-visible metadata plus the verifier the store keeps to itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredToken {
    record: ScopedTokenRecord,
    verifier: TokenVerifier,
}

/// A token to persist: its metadata plus the verifier derived from the secret shown once at mint time.
#[derive(Debug, Clone)]
pub struct NewScopedToken {
    pub name: TokenName,
    pub reach: GrantScope,
    pub actions: BTreeSet<Action>,
    pub expires_at: Option<i64>,
    pub verifier: TokenVerifier,
    pub created_by: UserId,
    pub created_at_unix: i64,
}

/// The effect of revoking a token: idempotent, so a repeat revoke reports the unchanged record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevokeScopedTokenOutcome {
    Revoked(ScopedTokenRecord),
    Unchanged(ScopedTokenRecord),
}

impl RevokeScopedTokenOutcome {
    #[must_use]
    pub const fn record(&self) -> &ScopedTokenRecord {
        match self {
            Self::Revoked(record) | Self::Unchanged(record) => record,
        }
    }
}

/// A bounded, cursor-paginated query over one reach's tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedTokenQuery {
    pub reach: GrantScope,
    pub cursor: Option<TokenId>,
    pub limit: usize,
}

/// One bounded page of token metadata in stable id order within a reach.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScopedTokenPage {
    pub tokens: Vec<ScopedTokenRecord>,
    pub next_cursor: Option<String>,
}

/// A rejected token query.
#[derive(Debug, thiserror::Error)]
pub enum ScopedTokenQueryError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("limit must be between 1 and {MAX_QUERY_LIMIT}")]
    InvalidLimit,
}

impl MetaStore {
    /// Persist a new token and index it by reach and by verifier in one transaction.
    ///
    /// # Errors
    /// Returns a store error when the row cannot be encoded or the transaction cannot commit.
    pub fn create_scoped_token(&self, request: NewScopedToken) -> Result<ScopedTokenRecord, MetaError> {
        let NewScopedToken {
            name,
            reach,
            actions,
            expires_at,
            verifier,
            created_by,
            created_at_unix,
        } = request;
        let record = ScopedTokenRecord {
            id: TokenId::random(),
            name,
            reach,
            actions,
            created_by,
            created_at_unix,
            expires_at,
            revoked_at: None,
            revision: 1,
        };
        let stored = StoredToken {
            record: record.clone(),
            verifier,
        };
        let txn = self.db.begin_write()?;
        {
            let bytes = serde_json::to_vec(&stored)?;
            txn.open_table(SCOPED_TOKEN)?
                .insert(record.id.as_str(), bytes.as_slice())?;
            txn.open_table(SCOPED_TOKEN_REACH)?
                .insert(reach_index_key(&record.reach, &record.id).as_str(), record.id.as_str())?;
            txn.open_table(SCOPED_TOKEN_VERIFIER)?
                .insert(stored.verifier.as_str(), record.id.as_str())?;
        }
        txn.commit()?;
        Ok(record)
    }

    /// Read one token's metadata by its stable id, revoked or live.
    ///
    /// # Errors
    /// Returns a store error when the row cannot be read or decoded.
    pub fn get_scoped_token(&self, id: &TokenId) -> Result<Option<ScopedTokenRecord>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(SCOPED_TOKEN) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(table
            .get(id.as_str())?
            .map(|value| serde_json::from_slice::<StoredToken>(value.value()))
            .transpose()?
            .map(|stored| stored.record))
    }

    /// Resolve the live token a presented secret authenticates, with one indexed read and no write.
    ///
    /// Returns `None` for a secret matching no token, and for one whose token is revoked (its verifier
    /// index entry was removed) or past its expiry.
    ///
    /// # Errors
    /// Returns a store error when the index or row cannot be read or decoded.
    pub fn verify_scoped_token(
        &self,
        presented: &TokenSecret,
        now: i64,
    ) -> Result<Option<ScopedTokenRecord>, MetaError> {
        let txn = self.db.begin_read()?;
        let verifiers = match txn.open_table(SCOPED_TOKEN_VERIFIER) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let Some(id) = verifiers
            .get(presented.verifier().as_str())?
            .map(|value| TokenId::new(value.value()))
        else {
            return Ok(None);
        };
        let tokens = match txn.open_table(SCOPED_TOKEN) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let Some(bytes) = tokens.get(id.as_str())? else {
            return Ok(None);
        };
        let stored: StoredToken = serde_json::from_slice(bytes.value())?;
        Ok(stored.record.is_live(now).then_some(stored.record))
    }

    /// List one reach's tokens in stable id order, resuming after `cursor`.
    ///
    /// # Errors
    /// Returns [`ScopedTokenQueryError::InvalidLimit`] for an out-of-range limit, or a store error when
    /// rows cannot be read or decoded.
    pub fn list_scoped_tokens(&self, query: &ScopedTokenQuery) -> Result<ScopedTokenPage, ScopedTokenQueryError> {
        if !(1..=MAX_QUERY_LIMIT).contains(&query.limit) {
            return Err(ScopedTokenQueryError::InvalidLimit);
        }
        let txn = self.db.begin_read().map_err(MetaError::from)?;
        let reach_index = match txn.open_table(SCOPED_TOKEN_REACH) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Ok(ScopedTokenPage {
                    tokens: Vec::new(),
                    next_cursor: None,
                });
            }
            Err(error) => return Err(MetaError::from(error).into()),
        };
        let tokens = match txn.open_table(SCOPED_TOKEN) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Ok(ScopedTokenPage {
                    tokens: Vec::new(),
                    next_cursor: None,
                });
            }
            Err(error) => return Err(MetaError::from(error).into()),
        };
        let prefix = reach_prefix(&query.reach);
        let start = query
            .cursor
            .as_ref()
            .map(|cursor| reach_index_key(&query.reach, cursor));
        let bound = start.as_deref().map_or(Included(prefix.as_str()), Excluded);
        let entries = reach_index.range::<&str>((bound, Unbounded)).map_err(MetaError::from)?;
        let mut records = Vec::with_capacity(query.limit + 1);
        for entry in entries {
            let (key, value) = entry.map_err(MetaError::from)?;
            if !key.value().starts_with(prefix.as_str()) {
                break;
            }
            let id = TokenId::new(value.value());
            let Some(bytes) = tokens.get(id.as_str()).map_err(MetaError::from)? else {
                continue;
            };
            let stored: StoredToken = serde_json::from_slice(bytes.value()).map_err(MetaError::from)?;
            records.push(stored.record);
            if records.len() > query.limit {
                break;
            }
        }
        let next_cursor = (records.len() > query.limit).then(|| records[query.limit - 1].id.to_string());
        records.truncate(query.limit);
        Ok(ScopedTokenPage {
            tokens: records,
            next_cursor,
        })
    }

    /// Replace a live token's verifier, bumping its revision and leaving its id and reach unchanged.
    ///
    /// Returns `None` when no token holds `id` or the token is revoked, leaving the prior state intact so
    /// a failed rotation never widens or clears access.
    ///
    /// # Errors
    /// Returns a store error when the row cannot be read, encoded, or committed.
    pub fn rotate_scoped_token(
        &self,
        id: &TokenId,
        verifier: &TokenVerifier,
    ) -> Result<Option<ScopedTokenRecord>, MetaError> {
        let txn = self.db.begin_write()?;
        let Some(mut stored) = read_stored(&txn, id)? else {
            return Ok(None);
        };
        if stored.record.revoked_at.is_some() {
            return Ok(None);
        }
        {
            let mut verifiers = txn.open_table(SCOPED_TOKEN_VERIFIER)?;
            verifiers.remove(stored.verifier.as_str())?;
            verifiers.insert(verifier.as_str(), id.as_str())?;
        }
        stored.verifier = verifier.clone();
        stored.record.revision += 1;
        let bytes = serde_json::to_vec(&stored)?;
        txn.open_table(SCOPED_TOKEN)?.insert(id.as_str(), bytes.as_slice())?;
        txn.commit()?;
        Ok(Some(stored.record))
    }

    /// Revoke a token, removing its verifier index so it stops authenticating at once. Idempotent: a
    /// repeat revoke reports the unchanged record.
    ///
    /// # Errors
    /// Returns a store error when the row cannot be read, encoded, or committed.
    pub fn revoke_scoped_token(&self, id: &TokenId, now: i64) -> Result<Option<RevokeScopedTokenOutcome>, MetaError> {
        let txn = self.db.begin_write()?;
        let Some(mut stored) = read_stored(&txn, id)? else {
            return Ok(None);
        };
        if stored.record.revoked_at.is_some() {
            return Ok(Some(RevokeScopedTokenOutcome::Unchanged(stored.record)));
        }
        txn.open_table(SCOPED_TOKEN_VERIFIER)?
            .remove(stored.verifier.as_str())?;
        stored.record.revoked_at = Some(now);
        stored.record.revision += 1;
        let bytes = serde_json::to_vec(&stored)?;
        txn.open_table(SCOPED_TOKEN)?.insert(id.as_str(), bytes.as_slice())?;
        txn.commit()?;
        Ok(Some(RevokeScopedTokenOutcome::Revoked(stored.record)))
    }
}

fn read_stored(txn: &WriteTransaction, id: &TokenId) -> Result<Option<StoredToken>, MetaError> {
    let table = txn.open_table(SCOPED_TOKEN)?;
    Ok(table
        .get(id.as_str())?
        .map(|value| serde_json::from_slice(value.value()))
        .transpose()?)
}

fn reach_key(reach: &GrantScope) -> String {
    match reach {
        GrantScope::Server => "server".to_owned(),
        GrantScope::Repository { name } => format!("repository/{name}"),
    }
}

/// The reach's index prefix. A NUL separates the reach from the id so no repository name is a prefix of
/// another (`root` versus `root/child`), which a bare `/` boundary would allow.
fn reach_prefix(reach: &GrantScope) -> String {
    format!("{}\0", reach_key(reach))
}

fn reach_index_key(reach: &GrantScope, id: &TokenId) -> String {
    format!("{}{}", reach_prefix(reach), id)
}
