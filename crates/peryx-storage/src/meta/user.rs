use peryx_identity::{
    PasswordVerifier, ServerUser, UserId, UserLifecycleChange, UserLifecycleEvent, UserName, UserNameError, UserState,
};
use redb::{ReadableTable as _, WriteTransaction};

use super::{MetaError, MetaStore, USER, USER_EVENT, USER_NAME, USER_VERIFIER};

/// A verifier and the serialized row identity required for conditional replacement.
pub struct StoredPasswordVerifier {
    verifier: PasswordVerifier,
    serialized: Vec<u8>,
}

impl StoredPasswordVerifier {
    #[must_use]
    pub const fn verifier(&self) -> &PasswordVerifier {
        &self.verifier
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UserStoreError {
    #[error(transparent)]
    Store(#[from] MetaError),
    #[error(transparent)]
    Name(#[from] UserNameError),
    #[error("user identity {canonical_name:?} already exists")]
    DuplicateName { canonical_name: String },
    #[error("server user {id} does not exist")]
    NotFound { id: UserId },
}

impl MetaStore {
    /// # Errors
    /// Returns a name error for an empty display name, a conflict for a canonical name already in
    /// use, or a store error when the transaction cannot commit.
    pub fn create_user(&self, display_name: &str) -> Result<ServerUser, UserStoreError> {
        let name = UserName::new(display_name)?;
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        {
            let names = txn.open_table(USER_NAME).map_err(MetaError::from)?;
            if names.get(name.canonical()).map_err(MetaError::from)?.is_some() {
                return Err(UserStoreError::DuplicateName {
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
        {
            let bytes = serde_json::to_vec(&user).map_err(MetaError::from)?;
            txn.open_table(USER)
                .map_err(MetaError::from)?
                .insert(user.id.as_str(), bytes.as_slice())
                .map_err(MetaError::from)?;
            txn.open_table(USER_NAME)
                .map_err(MetaError::from)?
                .insert(user.name.canonical(), user.id.as_str())
                .map_err(MetaError::from)?;
        }
        append_event(
            &txn,
            &UserLifecycleEvent {
                user_id: user.id.clone(),
                sequence: user.revision,
                change: UserLifecycleChange::Created {
                    display_name: user.name.display().to_owned(),
                },
            },
        )?;
        txn.commit().map_err(MetaError::from)?;
        Ok(user)
    }

    /// Includes disabled users.
    ///
    /// # Errors
    /// Returns a store error when the record cannot be read or decoded.
    pub fn get_user(&self, id: &UserId) -> Result<Option<ServerUser>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(USER) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(table
            .get(id.as_str())?
            .map(|value| serde_json::from_slice(value.value()))
            .transpose()?)
    }

    /// Uses the canonical name index and includes disabled users.
    ///
    /// # Errors
    /// Returns a name error for an empty lookup or a store error when the index or record cannot be
    /// read or decoded.
    pub fn get_user_by_name(&self, display_name: &str) -> Result<Option<ServerUser>, UserStoreError> {
        let name = UserName::new(display_name)?;
        let txn = self.db.begin_read().map_err(MetaError::from)?;
        let names = match txn.open_table(USER_NAME) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(error) => return Err(MetaError::from(error).into()),
        };
        let Some(id) = names
            .get(name.canonical())
            .map_err(MetaError::from)?
            .map(|value| value.value().to_owned())
        else {
            return Ok(None);
        };
        let users = match txn.open_table(USER) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(error) => return Err(MetaError::from(error).into()),
        };
        Ok(users
            .get(id.as_str())
            .map_err(MetaError::from)?
            .map(|value| serde_json::from_slice(value.value()))
            .transpose()
            .map_err(MetaError::from)?)
    }

    /// # Errors
    /// Returns a name error for an empty display name, a conflict when the new canonical name is in
    /// use, [`UserStoreError::NotFound`] for an unknown ID, or a store error when the transaction
    /// cannot commit.
    pub fn rename_user(&self, id: &UserId, display_name: &str) -> Result<ServerUser, UserStoreError> {
        let name = UserName::new(display_name)?;
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        let Some(mut user) = read_user(&txn, id)? else {
            return Err(UserStoreError::NotFound { id: id.clone() });
        };
        if user.name == name {
            return Ok(user);
        }
        if user.name.canonical() != name.canonical() {
            let mut names = txn.open_table(USER_NAME).map_err(MetaError::from)?;
            if names.get(name.canonical()).map_err(MetaError::from)?.is_some() {
                return Err(UserStoreError::DuplicateName {
                    canonical_name: name.canonical().to_owned(),
                });
            }
            names.remove(user.name.canonical()).map_err(MetaError::from)?;
            names
                .insert(name.canonical(), user.id.as_str())
                .map_err(MetaError::from)?;
        }
        let previous_display_name = user.name.display().to_owned();
        user.name = name;
        user.revision += 1;
        write_user(&txn, &user)?;
        append_event(
            &txn,
            &UserLifecycleEvent {
                user_id: user.id.clone(),
                sequence: user.revision,
                change: UserLifecycleChange::Renamed {
                    previous_display_name,
                    display_name: user.name.display().to_owned(),
                },
            },
        )?;
        txn.commit().map_err(MetaError::from)?;
        Ok(user)
    }

    /// # Errors
    /// Returns [`UserStoreError::NotFound`] for an unknown ID or a store error when the transaction
    /// cannot commit.
    pub fn set_user_state(&self, id: &UserId, state: UserState) -> Result<ServerUser, UserStoreError> {
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        let Some(mut user) = read_user(&txn, id)? else {
            return Err(UserStoreError::NotFound { id: id.clone() });
        };
        if user.state == state {
            return Ok(user);
        }
        user.state = state;
        user.revision += 1;
        write_user(&txn, &user)?;
        append_event(
            &txn,
            &UserLifecycleEvent {
                user_id: user.id.clone(),
                sequence: user.revision,
                change: match state {
                    UserState::Active => UserLifecycleChange::Reactivated,
                    UserState::Disabled => UserLifecycleChange::Disabled,
                },
            },
        )?;
        txn.commit().map_err(MetaError::from)?;
        Ok(user)
    }

    /// Replaces any prior verifier.
    ///
    /// # Errors
    /// Returns [`UserStoreError::NotFound`] for an unknown ID or a store error when the transaction
    /// cannot commit.
    pub fn set_user_password(&self, id: &UserId, verifier: &PasswordVerifier) -> Result<(), UserStoreError> {
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        if read_user(&txn, id)?.is_none() {
            return Err(UserStoreError::NotFound { id: id.clone() });
        }
        let bytes = serde_json::to_vec(verifier).map_err(MetaError::from)?;
        txn.open_table(USER_VERIFIER)
            .map_err(MetaError::from)?
            .insert(id.as_str(), bytes.as_slice())
            .map_err(MetaError::from)?;
        txn.commit().map_err(MetaError::from)?;
        Ok(())
    }

    /// Returns `None` when the user has no password enrolled.
    ///
    /// # Errors
    /// Returns a store error when the record cannot be read or decoded.
    pub fn get_user_password(&self, id: &UserId) -> Result<Option<StoredPasswordVerifier>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(USER_VERIFIER) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let Some(value) = table.get(id.as_str())? else {
            return Ok(None);
        };
        let serialized = value.value().to_vec();
        Ok(Some(StoredPasswordVerifier {
            verifier: serde_json::from_slice(&serialized)?,
            serialized,
        }))
    }

    /// Replaces the verifier only when it still matches the value the caller checked.
    ///
    /// # Errors
    /// Returns a store error when the transaction cannot read or replace the row.
    pub fn compare_and_set_user_password(
        &self,
        id: &UserId,
        expected: &StoredPasswordVerifier,
        replacement: &PasswordVerifier,
    ) -> Result<bool, MetaError> {
        let txn = self.db.begin_write()?;
        {
            let mut table = txn.open_table(USER_VERIFIER)?;
            if table
                .get(id.as_str())?
                .is_none_or(|value| value.value() != expected.serialized)
            {
                return Ok(false);
            }
            let bytes = serde_json::to_vec(replacement)?;
            table.insert(id.as_str(), bytes.as_slice())?;
        }
        txn.commit()?;
        Ok(true)
    }

    /// Disables password authentication for the user.
    ///
    /// # Errors
    /// Returns [`UserStoreError::NotFound`] for an unknown ID or a store error when the transaction
    /// cannot commit.
    pub fn clear_user_password(&self, id: &UserId) -> Result<(), UserStoreError> {
        let txn = self.db.begin_write().map_err(MetaError::from)?;
        if read_user(&txn, id)?.is_none() {
            return Err(UserStoreError::NotFound { id: id.clone() });
        }
        txn.open_table(USER_VERIFIER)
            .map_err(MetaError::from)?
            .remove(id.as_str())
            .map_err(MetaError::from)?;
        txn.commit().map_err(MetaError::from)?;
        Ok(())
    }

    /// Returns lifecycle events in commit order.
    ///
    /// # Errors
    /// Returns a store error when the records cannot be read or decoded.
    pub fn user_events(&self, id: &UserId) -> Result<Vec<UserLifecycleEvent>, MetaError> {
        let txn = self.db.begin_read()?;
        let table = match txn.open_table(USER_EVENT) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let start = format!("{id}/");
        let end = format!("{id}/~");
        let mut events = Vec::new();
        for entry in table.range(start.as_str()..end.as_str())? {
            let (_, value) = entry?;
            events.push(serde_json::from_slice(value.value())?);
        }
        Ok(events)
    }
}

pub(super) fn read_user(txn: &WriteTransaction, id: &UserId) -> Result<Option<ServerUser>, MetaError> {
    let table = txn.open_table(USER)?;
    Ok(table
        .get(id.as_str())?
        .map(|value| serde_json::from_slice(value.value()))
        .transpose()?)
}

pub(super) fn write_user(txn: &WriteTransaction, user: &ServerUser) -> Result<(), MetaError> {
    let bytes = serde_json::to_vec(user)?;
    txn.open_table(USER)?.insert(user.id.as_str(), bytes.as_slice())?;
    Ok(())
}

pub(super) fn append_event(txn: &WriteTransaction, event: &UserLifecycleEvent) -> Result<(), MetaError> {
    let key = format!("{}/{:020}", event.user_id, event.sequence);
    let bytes = serde_json::to_vec(event)?;
    txn.open_table(USER_EVENT)?.insert(key.as_str(), bytes.as_slice())?;
    Ok(())
}
