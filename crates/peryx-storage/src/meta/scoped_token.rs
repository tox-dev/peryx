//! Stores only token verifier digests, never plaintext secrets. Revocation removes the verifier index at
//! once while retaining lifecycle records for audit.

use std::collections::BTreeSet;
use std::ops::Bound::{Excluded, Included, Unbounded};

use peryx_identity::{Action, GrantScope, TokenId, TokenName, TokenSecret, TokenVerifier, UserId};
use redb::{ReadableTable as _, WriteTransaction};
use serde::{Deserialize, Serialize};

use super::{MetaError, MetaStore, SCOPED_TOKEN, SCOPED_TOKEN_REACH, SCOPED_TOKEN_VERIFIER};

const MAX_QUERY_LIMIT: usize = 100;

/// Safe for management API responses because it excludes the secret and verifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopedTokenRecord {
    pub id: TokenId,
    pub name: TokenName,
    pub reach: GrantScope,
    pub actions: BTreeSet<Action>,
    pub created_by: UserId,
    pub created_at_unix: i64,
    /// `None` never expires.
    pub expires_at: Option<i64>,
    /// `None` while live.
    pub revoked_at: Option<i64>,
    pub revision: u64,
}

impl ScopedTokenRecord {
    #[must_use]
    pub fn is_live(&self, now: i64) -> bool {
        self.revoked_at.is_none() && self.expires_at.is_none_or(|expiry| now < expiry)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredToken {
    record: ScopedTokenRecord,
    verifier: TokenVerifier,
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedTokenQuery {
    pub reach: GrantScope,
    pub cursor: Option<TokenId>,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScopedTokenPage {
    pub tokens: Vec<ScopedTokenRecord>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ScopedTokenQueryError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("limit must be between 1 and {MAX_QUERY_LIMIT}")]
    InvalidLimit,
}

impl MetaStore {
    /// Commits the token and its reach and verifier indexes atomically.
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

    /// Includes revoked tokens.
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

    /// Performs one indexed read and no write. Returns `None` for unknown, revoked, or expired tokens.
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

    /// Returns one reach's tokens in stable ID order after `cursor`.
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

    /// Replaces a live verifier and bumps its revision without changing ID or reach. Missing and revoked
    /// tokens return `None` without changing access.
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

    /// Removes the verifier index at once; repeated revocation returns the unchanged record.
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

/// A NUL separator prevents repository names such as `root` and `root/child` from sharing a prefix.
fn reach_prefix(reach: &GrantScope) -> String {
    format!("{}\0", reach_key(reach))
}

fn reach_index_key(reach: &GrantScope, id: &TokenId) -> String {
    format!("{}{}", reach_prefix(reach), id)
}
