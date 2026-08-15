use std::collections::BTreeMap;

use peryx_identity::{
    ExternalIdentity, ExternalIdentityResolution, ExternalIdentityStore, ExternalLinkRequest, GrantScope,
    ManagedRoleGrant, RoleGrant, ServerUser, UserId, UserLifecycleChange, UserLifecycleEvent, UserName, UserState,
};
use redb::{ReadableTable as _, WriteTransaction};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::user::{append_event, read_user, write_user};
use super::{EXTERNAL_IDENTITY, EXTERNAL_ROLE_GRANT, MetaError, MetaStore, USER_NAME};

const KEY_DOMAIN: &[u8] = b"peryx.external-identity.v1\0";

#[derive(Debug, Serialize, Deserialize)]
struct StoredExternalIdentityLink {
    id: String,
    identity: ExternalIdentity,
    user_id: UserId,
}

#[derive(Debug, thiserror::Error)]
pub enum ExternalIdentityStoreError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error("external identity link key conflicts with another identity")]
    KeyCollision,
    #[error("external identity link references missing server user {id}")]
    MissingUser { id: UserId },
    #[error("external identity link references disabled server user {id}")]
    DisabledUser { id: UserId },
}

impl MetaStore {
    /// Creates a stable local user on first login and replaces only grants owned by this link.
    ///
    /// # Errors
    /// Returns an integrity error for a conflicting or dangling link, a disabled-user error when the
    /// linked account cannot authenticate, or a store error when the transaction cannot commit.
    pub fn link_external_identity(
        &self,
        request: ExternalLinkRequest,
    ) -> Result<ExternalIdentityResolution, ExternalIdentityStoreError> {
        let ExternalLinkRequest {
            identity,
            display_name,
            grants,
        } = request;
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        let key = identity_key(&identity);
        let (link, link_created) = if let Some(link) = read_link(&txn, &key)? {
            if link.identity != identity {
                return Err(ExternalIdentityStoreError::KeyCollision);
            }
            (link, false)
        } else {
            create_link(&txn, &identity, &display_name)?
        };
        let user = read_user(&txn, &link.user_id)?.ok_or_else(|| ExternalIdentityStoreError::MissingUser {
            id: link.user_id.clone(),
        })?;
        if user.state == UserState::Disabled {
            return Err(ExternalIdentityStoreError::DisabledUser { id: user.id });
        }
        let grants_changed = replace_managed_grants(&txn, &link, &grants)?;
        txn.commit().map_err(MetaError::from)?;
        let provider = identity.provider.as_str();
        let user_id = user.id.as_str();
        let outcome = if link_created { "created" } else { "resolved" };
        let managed_grants = grants.len();
        tracing::info!(
            target: "peryx::security",
            security_event = true,
            event = "external_identity",
            provider,
            user = user_id,
            result = outcome,
            managed_grants,
            grants_changed,
            "external identity resolved"
        );
        Ok(ExternalIdentityResolution {
            user,
            link_created,
            grants_changed,
        })
    }

    /// # Errors
    /// Returns a collision error when the indexed record holds another identity or a store error when
    /// the record cannot be read.
    pub fn external_identity_user(
        &self,
        identity: &ExternalIdentity,
    ) -> Result<Option<UserId>, ExternalIdentityStoreError> {
        let txn = self.db.begin_read().map_err(MetaError::from)?;
        let table = match txn.open_table(EXTERNAL_IDENTITY) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(error) => return Err(MetaError::from(error).into()),
        };
        let key = identity_key(identity);
        let Some(value) = table.get(key.as_slice()).map_err(MetaError::from)? else {
            return Ok(None);
        };
        let link: StoredExternalIdentityLink = serde_json::from_slice(value.value()).map_err(MetaError::from)?;
        if link.identity != *identity {
            return Err(ExternalIdentityStoreError::KeyCollision);
        }
        Ok(Some(link.user_id))
    }
}

impl ExternalIdentityStore for MetaStore {
    type Error = ExternalIdentityStoreError;

    fn link_or_resolve(&self, request: ExternalLinkRequest) -> Result<ExternalIdentityResolution, Self::Error> {
        self.link_external_identity(request)
    }
}

fn read_link(txn: &WriteTransaction, key: &[u8; 32]) -> Result<Option<StoredExternalIdentityLink>, MetaError> {
    Ok(txn
        .open_table(EXTERNAL_IDENTITY)?
        .get(key.as_slice())?
        .map(|value| serde_json::from_slice(value.value()))
        .transpose()?)
}

fn create_link(
    txn: &WriteTransaction,
    identity: &ExternalIdentity,
    display_name: &UserName,
) -> Result<(StoredExternalIdentityLink, bool), MetaError> {
    let user = create_external_user(txn, display_name)?;
    write_user(txn, &user)?;
    txn.open_table(USER_NAME)?
        .insert(user.name.canonical(), user.id.as_str())?;
    let event = UserLifecycleEvent {
        user_id: user.id.clone(),
        sequence: user.revision,
        change: UserLifecycleChange::Created {
            display_name: user.name.display().to_owned(),
        },
    };
    append_event(txn, &event)?;
    let link = StoredExternalIdentityLink {
        id: format!("ext_{}", uuid::Uuid::new_v4().simple()),
        identity: identity.clone(),
        user_id: user.id,
    };
    let bytes = serde_json::to_vec(&link)?;
    let key = identity_key(&link.identity);
    txn.open_table(EXTERNAL_IDENTITY)?
        .insert(key.as_slice(), bytes.as_slice())?;
    Ok((link, true))
}

fn create_external_user(txn: &WriteTransaction, requested: &UserName) -> Result<ServerUser, MetaError> {
    let id = UserId::random();
    let names = txn.open_table(USER_NAME)?;
    let name = if names.get(requested.canonical())?.is_none() {
        requested.clone()
    } else {
        requested.with_id_suffix(&id)
    };
    Ok(ServerUser {
        id,
        name,
        state: UserState::Active,
        revision: 1,
    })
}

fn replace_managed_grants(
    txn: &WriteTransaction,
    link: &StoredExternalIdentityLink,
    requested: &[ManagedRoleGrant],
) -> Result<bool, MetaError> {
    let (start, end) = managed_prefix_bounds(&link.user_id, &link.id);
    let mut table = txn.open_table(EXTERNAL_ROLE_GRANT)?;
    let existing = table
        .range(start.as_str()..end.as_str())?
        .map(|entry| {
            let (key, value) = entry?;
            Ok((
                key.value().to_owned(),
                serde_json::from_slice::<RoleGrant>(value.value())?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>, MetaError>>()?;
    let desired = requested
        .iter()
        .map(|grant| {
            let grant = RoleGrant::new(link.user_id.clone(), grant.role, grant.scope.clone());
            (managed_grant_key(&link.id, &grant), grant)
        })
        .collect::<BTreeMap<_, _>>();
    if existing == desired {
        return Ok(false);
    }
    for key in existing.keys() {
        table.remove(key.as_str())?;
    }
    for (key, grant) in desired {
        let bytes = serde_json::to_vec(&grant)?;
        table.insert(key.as_str(), bytes.as_slice())?;
    }
    Ok(true)
}

fn identity_key(identity: &ExternalIdentity) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(KEY_DOMAIN);
    digest.update(
        u64::try_from(identity.provider.as_str().len())
            .expect("provider IDs are bounded below u64")
            .to_be_bytes(),
    );
    digest.update(identity.provider.as_str());
    digest.update(
        u64::try_from(identity.subject.as_str().len())
            .expect("external subjects are bounded below u64")
            .to_be_bytes(),
    );
    digest.update(identity.subject.as_str());
    digest.finalize().into()
}

fn managed_prefix_bounds(user: &UserId, link_id: &str) -> (String, String) {
    (format!("{user}/{link_id}/"), format!("{user}/{link_id}0"))
}

fn managed_grant_key(link_id: &str, grant: &RoleGrant) -> String {
    let reach = match &grant.scope {
        GrantScope::Server => "server".to_owned(),
        GrantScope::Repository { name } => format!("repository/{name}"),
    };
    format!("{}/{link_id}/{}/{reach}", grant.user, grant.role.as_str())
}
