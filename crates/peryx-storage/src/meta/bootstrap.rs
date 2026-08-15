use peryx_identity::{
    GrantScope, PasswordVerifier, Role, RoleGrant, ServerUser, UserId, UserLifecycleChange, UserLifecycleEvent,
    UserName, UserNameError, UserState,
};
use redb::ReadableTable as _;

use super::{MetaError, MetaStore, ROLE_GRANT, USER, USER_EVENT, USER_NAME, USER_VERIFIER};

#[derive(Debug, thiserror::Error)]
pub enum AdministratorBootstrapError {
    #[error("an administrator grant already exists")]
    AdministratorExists,
    #[error("user identity {canonical_name:?} already exists")]
    DuplicateName { canonical_name: String },
    #[error(transparent)]
    Name(#[from] UserNameError),
    #[error(transparent)]
    Store(#[from] MetaError),
}

impl MetaStore {
    /// Creates the first administrator and its credentials, grant, and lifecycle record atomically.
    /// Redb serialization gives concurrent attempts one winner.
    ///
    /// # Errors
    /// Returns [`AdministratorBootstrapError::AdministratorExists`] when any administrator grant is
    /// present, a name conflict for an existing identity, or a store error when the transaction aborts.
    pub fn bootstrap_administrator(
        &self,
        display_name: &str,
        verifier: &PasswordVerifier,
    ) -> Result<ServerUser, AdministratorBootstrapError> {
        let name = UserName::new(display_name)?;
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        {
            let grants = txn.open_table(ROLE_GRANT).map_err(MetaError::from)?;
            for entry in grants.iter().map_err(MetaError::from)? {
                let (_, value) = entry.map_err(MetaError::from)?;
                (serde_json::from_slice::<RoleGrant>(value.value())
                    .map_err(MetaError::from)?
                    .role
                    != Role::Administrator)
                    .then_some(())
                    .ok_or(AdministratorBootstrapError::AdministratorExists)?;
            }
        }
        {
            let names = txn.open_table(USER_NAME).map_err(MetaError::from)?;
            if names.get(name.canonical()).map_err(MetaError::from)?.is_some() {
                return Err(AdministratorBootstrapError::DuplicateName {
                    canonical_name: name.canonical().to_owned(),
                });
            }
        }
        let user = ServerUser {
            id: UserId::random(),
            name,
            state: UserState::Active,
            revision: 1,
        };
        let user_bytes = serde_json::to_vec(&user).map_err(MetaError::from)?;
        txn.open_table(USER)
            .map_err(MetaError::from)?
            .insert(user.id.as_str(), user_bytes.as_slice())
            .map_err(MetaError::from)?;
        txn.open_table(USER_NAME)
            .map_err(MetaError::from)?
            .insert(user.name.canonical(), user.id.as_str())
            .map_err(MetaError::from)?;
        let event = UserLifecycleEvent {
            user_id: user.id.clone(),
            sequence: user.revision,
            change: UserLifecycleChange::AdministratorBootstrapped {
                display_name: user.name.display().to_owned(),
            },
        };
        let event_bytes = serde_json::to_vec(&event).map_err(MetaError::from)?;
        txn.open_table(USER_EVENT)
            .map_err(MetaError::from)?
            .insert(
                format!("{}/{:020}", event.user_id, event.sequence).as_str(),
                event_bytes.as_slice(),
            )
            .map_err(MetaError::from)?;
        let verifier_bytes = serde_json::to_vec(verifier).map_err(MetaError::from)?;
        txn.open_table(USER_VERIFIER)
            .map_err(MetaError::from)?
            .insert(user.id.as_str(), verifier_bytes.as_slice())
            .map_err(MetaError::from)?;
        let grant = RoleGrant::new(user.id.clone(), Role::Administrator, GrantScope::Server);
        super::role_grant::write_grant(&txn, &super::StoredRoleGrant::provisioned(grant))?;
        txn.commit().map_err(MetaError::from)?;
        Ok(user)
    }
}
