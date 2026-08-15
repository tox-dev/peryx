use std::collections::BTreeMap;

use peryx_identity::{GrantScope, Role, RoleGrant, ServerUser, UserId, UserState};
use redb::{ReadableTable as _, WriteTransaction};

use super::{EXTERNAL_ROLE_GRANT, MetaError, MetaStore, ROLE_GRANT, ROLE_GRANT_BY_SCOPE, USER};

#[derive(Debug, thiserror::Error)]
pub enum RoleGrantStoreError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("server user {id} does not exist")]
    UnknownUser { id: UserId },
    #[error("server user {id} is disabled")]
    DisabledUser { id: UserId },
}

/// Audit fields default for records written before managed grants existed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoredRoleGrant {
    #[serde(flatten)]
    pub grant: RoleGrant,
    /// Advances on each write for conditional revocation.
    #[serde(default)]
    pub version: u64,
    /// `None` for bootstrap and group-sync bindings.
    #[serde(default)]
    pub granted_by: Option<UserId>,
    /// `None` for bootstrap and group-sync bindings.
    #[serde(default)]
    pub granted_at_unix: Option<i64>,
}

impl StoredRoleGrant {
    pub(super) const fn provisioned(grant: RoleGrant) -> Self {
        Self {
            grant,
            version: 1,
            granted_by: None,
            granted_at_unix: None,
        }
    }

    /// Encodes the primary key into a stable identifier unique across users, roles, and reaches.
    #[must_use]
    pub fn id(&self) -> String {
        encode_grant_id(&primary_key(&self.grant))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateGrantOutcome {
    pub record: StoredRoleGrant,
    pub created: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// [`Self::Removed`] retains the final record for audit.
pub enum DeleteGrantOutcome {
    NotFound,
    PreconditionFailed { current: u64 },
    Removed(StoredRoleGrant),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleGrantFilter {
    All,
    User(UserId),
    Resource(GrantScope),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleGrantQuery {
    pub filter: RoleGrantFilter,
    /// Exclusive key from the previous page.
    pub cursor: Option<String>,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct RoleGrantPage {
    pub grants: Vec<StoredRoleGrant>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RoleGrantQueryError {
    #[error("limit must be between 1 and 100")]
    InvalidLimit,
    #[error(transparent)]
    Store(#[from] MetaError),
}

const MAX_LIMIT: usize = 100;

impl MetaStore {
    /// Treats an existing binding as success without changing its version or audit fields.
    ///
    /// # Errors
    /// Returns [`RoleGrantStoreError::UnknownUser`] when no server user holds `id`, or a store error
    /// when the transaction cannot commit.
    pub fn grant_role(&self, id: &UserId, role: Role, scope: GrantScope) -> Result<RoleGrant, RoleGrantStoreError> {
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        if !user_exists(&txn, id)? {
            return Err(RoleGrantStoreError::UnknownUser { id: id.clone() });
        }
        let grant = RoleGrant::new(id.clone(), role, scope);
        // Preserving an existing version prevents stale provisioning from defeating concurrency checks.
        if read_grant(&txn, &primary_key(&grant))?.is_none() {
            write_grant(&txn, &StoredRoleGrant::provisioned(grant.clone()))?;
            txn.commit().map_err(MetaError::from)?;
        }
        Ok(grant)
    }

    /// Reasserting a binding advances its version and refreshes audit fields.
    ///
    /// # Errors
    /// Returns [`RoleGrantStoreError::UnknownUser`] for a missing user, [`RoleGrantStoreError::DisabledUser`]
    /// for a disabled one, or a store error when the transaction cannot commit.
    pub fn create_managed_grant(
        &self,
        grant: &RoleGrant,
        granted_by: &UserId,
        now: i64,
    ) -> Result<CreateGrantOutcome, RoleGrantStoreError> {
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        match user_state(&txn, &grant.user)? {
            None => return Err(RoleGrantStoreError::UnknownUser { id: grant.user.clone() }),
            Some(UserState::Disabled) => return Err(RoleGrantStoreError::DisabledUser { id: grant.user.clone() }),
            Some(UserState::Active) => {}
        }
        let existing = read_grant(&txn, &primary_key(grant))?;
        let created = existing.is_none();
        let record = StoredRoleGrant {
            grant: grant.clone(),
            version: existing.map_or(1, |prior| prior.version + 1),
            granted_by: Some(granted_by.clone()),
            granted_at_unix: Some(now),
        };
        write_grant(&txn, &record)?;
        txn.commit().map_err(MetaError::from)?;
        Ok(CreateGrantOutcome { record, created })
    }

    /// Returns whether the binding existed.
    ///
    /// # Errors
    /// Returns a store error when the transaction cannot commit.
    pub fn revoke_role(&self, id: &UserId, role: Role, scope: &GrantScope) -> Result<bool, MetaError> {
        let txn = self.db.begin_write()?;
        let removed = remove_grant(&txn, &RoleGrant::new(id.clone(), role, scope.clone()))?.is_some();
        txn.commit()?;
        Ok(removed)
    }

    /// Returns `None` for malformed and absent identifiers.
    ///
    /// # Errors
    /// Returns a store error when the record cannot be read or decoded.
    pub fn managed_grant(&self, id: &str) -> Result<Option<StoredRoleGrant>, MetaError> {
        let Some(key) = decode_grant_id(id) else {
            return Ok(None);
        };
        let txn = self.db.begin_read()?;
        match txn.open_table(ROLE_GRANT) {
            Ok(table) => Ok(table
                .get(key.as_str())?
                .map(|value| serde_json::from_slice(value.value()))
                .transpose()?),
            Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Removes the binding only at `expected_version`, so a concurrent reassertion wins safely.
    ///
    /// # Errors
    /// Returns a store error when the transaction cannot commit.
    pub fn delete_managed_grant(&self, id: &str, expected_version: u64) -> Result<DeleteGrantOutcome, MetaError> {
        let Some(key) = decode_grant_id(id) else {
            return Ok(DeleteGrantOutcome::NotFound);
        };
        let txn = self.db.begin_write()?;
        let outcome = match read_grant(&txn, &key)? {
            None => DeleteGrantOutcome::NotFound,
            Some(stored) if stored.version != expected_version => DeleteGrantOutcome::PreconditionFailed {
                current: stored.version,
            },
            Some(stored) => {
                remove_grant(&txn, &stored.grant)?;
                DeleteGrantOutcome::Removed(stored)
            }
        };
        txn.commit()?;
        Ok(outcome)
    }

    /// Uses the matching ordered index for bounded user or resource listings.
    ///
    /// # Errors
    /// Returns [`RoleGrantQueryError::InvalidLimit`] when the limit is zero or above the cap, or a store
    /// error when a record cannot be read or decoded.
    pub fn list_managed_grants(&self, query: &RoleGrantQuery) -> Result<RoleGrantPage, RoleGrantQueryError> {
        if query.limit == 0 || query.limit > MAX_LIMIT {
            return Err(RoleGrantQueryError::InvalidLimit);
        }
        let txn = self.db.begin_read().map_err(MetaError::from)?;
        let (table, prefix) = match &query.filter {
            RoleGrantFilter::All => (ROLE_GRANT, None),
            RoleGrantFilter::User(id) => (ROLE_GRANT, Some(format!("{id}/"))),
            RoleGrantFilter::Resource(scope) => (ROLE_GRANT_BY_SCOPE, Some(format!("{}\u{0}", reach_of(scope)))),
        };
        let mut page = Vec::new();
        let mut overflow = None;
        match txn.open_table(table) {
            Ok(table) => {
                for entry in table.iter().map_err(MetaError::from)? {
                    let (key, value) = entry.map_err(MetaError::from)?;
                    let key = key.value().to_owned();
                    if prefix.as_ref().is_some_and(|prefix| !key.starts_with(prefix)) {
                        continue;
                    }
                    if query.cursor.as_deref().is_some_and(|cursor| key.as_str() <= cursor) {
                        continue;
                    }
                    if page.len() == query.limit {
                        overflow = Some(key);
                        break;
                    }
                    page.push((key, serde_json::from_slice(value.value()).map_err(MetaError::from)?));
                }
            }
            Err(redb::TableError::TableDoesNotExist(_)) => {}
            Err(error) => return Err(MetaError::from(error).into()),
        }
        let next_cursor = overflow.and_then(|_| page.last().map(|(key, _)| key.clone()));
        Ok(RoleGrantPage {
            grants: page.into_iter().map(|(_, grant)| grant).collect(),
            next_cursor,
        })
    }

    /// Returns an empty list for unknown users and users without grants.
    ///
    /// # Errors
    /// Returns a store error when a record cannot be read or decoded.
    pub fn user_role_grants(&self, id: &UserId) -> Result<Vec<RoleGrant>, MetaError> {
        let txn = self.db.begin_read()?;
        let (start, end) = (format!("{id}/"), format!("{id}0"));
        let mut grants = BTreeMap::new();
        match txn.open_table(ROLE_GRANT) {
            Ok(table) => {
                for entry in table.range(start.as_str()..end.as_str())? {
                    let (key, value) = entry?;
                    grants.insert(key.value().to_owned(), decode_binding(value.value())?);
                }
            }
            Err(redb::TableError::TableDoesNotExist(_)) => {}
            Err(error) => return Err(error.into()),
        }
        match txn.open_table(EXTERNAL_ROLE_GRANT) {
            Ok(table) => {
                for entry in table.range(start.as_str()..end.as_str())? {
                    let (_, value) = entry?;
                    let grant = decode_binding(value.value())?;
                    grants.entry(primary_key(&grant)).or_insert(grant);
                }
            }
            Err(redb::TableError::TableDoesNotExist(_)) => {}
            Err(error) => return Err(error.into()),
        }
        Ok(grants.into_values().collect())
    }
}

/// Recovers the reach without a store read so routes can authorize before lookup. Malformed IDs return
/// `None`.
#[must_use]
pub fn role_grant_reach(id: &str) -> Option<GrantScope> {
    let key = decode_grant_id(id)?;
    let mut parts = key.splitn(3, '/');
    let (Some(_user), Some(_role), Some(reach)) = (parts.next(), parts.next(), parts.next()) else {
        return None;
    };
    if reach == "server" {
        return Some(GrantScope::Server);
    }
    let name = reach.strip_prefix("repository/")?;
    (!name.is_empty()).then(|| GrantScope::Repository { name: name.to_owned() })
}

pub(super) fn write_grant(txn: &WriteTransaction, stored: &StoredRoleGrant) -> Result<(), MetaError> {
    let bytes = serde_json::to_vec(stored)?;
    txn.open_table(ROLE_GRANT)?
        .insert(primary_key(&stored.grant).as_str(), bytes.as_slice())?;
    txn.open_table(ROLE_GRANT_BY_SCOPE)?
        .insert(scope_key(&stored.grant).as_str(), bytes.as_slice())?;
    Ok(())
}

fn remove_grant(txn: &WriteTransaction, grant: &RoleGrant) -> Result<Option<StoredRoleGrant>, MetaError> {
    let removed = txn
        .open_table(ROLE_GRANT)?
        .remove(primary_key(grant).as_str())?
        .map(|value| serde_json::from_slice(value.value()))
        .transpose()?;
    txn.open_table(ROLE_GRANT_BY_SCOPE)?.remove(scope_key(grant).as_str())?;
    Ok(removed)
}

fn read_grant(txn: &WriteTransaction, key: &str) -> Result<Option<StoredRoleGrant>, MetaError> {
    Ok(txn
        .open_table(ROLE_GRANT)?
        .get(key)?
        .map(|value| serde_json::from_slice(value.value()))
        .transpose()?)
}

fn decode_binding(value: &[u8]) -> Result<RoleGrant, MetaError> {
    Ok(serde_json::from_slice(value)?)
}

fn user_exists(txn: &WriteTransaction, id: &UserId) -> Result<bool, MetaError> {
    Ok(user_state(txn, id)?.is_some())
}

fn user_state(txn: &WriteTransaction, id: &UserId) -> Result<Option<UserState>, MetaError> {
    Ok(txn
        .open_table(USER)?
        .get(id.as_str())?
        .map(|value| serde_json::from_slice::<ServerUser>(value.value()))
        .transpose()?
        .map(|user| user.state))
}

fn primary_key(grant: &RoleGrant) -> String {
    format!("{}/{}/{}", grant.user, grant.role.as_str(), reach_of(&grant.scope))
}

fn scope_key(grant: &RoleGrant) -> String {
    format!(
        "{}\u{0}{}\u{0}{}",
        reach_of(&grant.scope),
        grant.user,
        grant.role.as_str()
    )
}

fn reach_of(scope: &GrantScope) -> String {
    match scope {
        GrantScope::Server => "server".to_owned(),
        GrantScope::Repository { name } => format!("repository/{name}"),
    }
}

fn encode_grant_id(key: &str) -> String {
    use std::fmt::Write as _;
    key.bytes().fold(String::from("rg_"), |mut id, byte| {
        write!(id, "{byte:02x}").expect("writing hex to a String never fails");
        id
    })
}

fn decode_grant_id(id: &str) -> Option<String> {
    let hex = id.strip_prefix("rg_")?;
    if hex.len() % 2 != 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for index in (0..hex.len()).step_by(2) {
        bytes.push(u8::from_str_radix(hex.get(index..index + 2)?, 16).ok()?);
    }
    String::from_utf8(bytes).ok()
}
