use std::sync::Arc;

use peryx_identity::{
    PasswordCheck, PasswordError, PasswordPolicy, PasswordVerifier, ServerUser, UserId, UserLifecycleEvent, UserState,
};
use peryx_storage::meta::{AdministratorBootstrapError, MetaError, MetaStore, UserStoreError};
use tokio::sync::Semaphore;

/// How many password derivations may run at once by default, chosen well under the request worker
/// count so a burst of logins cannot starve request serving.
const DEFAULT_PASSWORD_CHECKS: usize = 4;

/// Persistent users and bounded local-password authentication.
#[derive(Debug, Clone)]
pub struct UserService {
    store: MetaStore,
    policy: PasswordPolicy,
    verifications: Arc<Semaphore>,
}

/// A rejected password enrollment.
#[derive(Debug, thiserror::Error)]
pub enum EnrollError {
    #[error(transparent)]
    Hash(#[from] PasswordError),
    #[error(transparent)]
    Store(#[from] UserStoreError),
}

/// A rejected first-administrator bootstrap.
#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error(transparent)]
    Hash(#[from] PasswordError),
    #[error(transparent)]
    Store(#[from] AdministratorBootstrapError),
}

impl UserService {
    #[must_use]
    pub fn new(store: MetaStore) -> Self {
        Self::with_password_settings(store, PasswordPolicy::recommended(), DEFAULT_PASSWORD_CHECKS)
    }

    #[must_use]
    pub fn with_password_settings(store: MetaStore, policy: PasswordPolicy, max_concurrent_checks: usize) -> Self {
        Self {
            store,
            policy,
            verifications: Arc::new(Semaphore::new(max_concurrent_checks)),
        }
    }

    /// # Errors
    /// Returns a validation, uniqueness, or storage error.
    pub fn create(&self, display_name: &str) -> Result<ServerUser, UserStoreError> {
        self.store.create_user(display_name)
    }

    /// # Errors
    /// Returns a storage error when the user cannot be read.
    pub fn inspect(&self, id: &UserId) -> Result<Option<ServerUser>, MetaError> {
        self.store.get_user(id)
    }

    /// Disabled users remain inspectable but do not resolve through this operation.
    ///
    /// # Errors
    /// Returns a validation or storage error when the lookup cannot be completed.
    pub fn identify(&self, display_name: &str) -> Result<Option<ServerUser>, UserStoreError> {
        Ok(self
            .store
            .get_user_by_name(display_name)?
            .filter(|user| user.state == UserState::Active))
    }

    /// # Errors
    /// Returns a validation, uniqueness, missing-user, or storage error.
    pub fn rename(&self, id: &UserId, display_name: &str) -> Result<ServerUser, UserStoreError> {
        self.store.rename_user(id, display_name)
    }

    /// # Errors
    /// Returns a missing-user or storage error.
    pub fn disable(&self, id: &UserId) -> Result<ServerUser, UserStoreError> {
        self.store.set_user_state(id, UserState::Disabled)
    }

    /// # Errors
    /// Returns a missing-user or storage error.
    pub fn reactivate(&self, id: &UserId) -> Result<ServerUser, UserStoreError> {
        self.store.set_user_state(id, UserState::Active)
    }

    /// # Errors
    /// Returns a storage error when the events cannot be read.
    pub fn events(&self, id: &UserId) -> Result<Vec<UserLifecycleEvent>, MetaError> {
        self.store.user_events(id)
    }

    /// # Errors
    /// Returns [`EnrollError::Hash`] when derivation fails and [`EnrollError::Store`] for an unknown
    /// user or a storage failure.
    pub async fn set_password(&self, id: &UserId, password: &str) -> Result<(), EnrollError> {
        let verifier = self.hash(password.to_owned()).await?;
        self.store.set_user_password(id, &verifier)?;
        Ok(())
    }

    /// # Errors
    /// Returns [`BootstrapError::Hash`] when derivation fails or [`BootstrapError::Store`] when an
    /// administrator already exists, the identity conflicts, or the metadata transaction aborts.
    pub async fn bootstrap_administrator(
        &self,
        display_name: &str,
        password: &str,
    ) -> Result<ServerUser, BootstrapError> {
        let verifier = self.hash(password.to_owned()).await?;
        Ok(self.store.bootstrap_administrator(display_name, &verifier)?)
    }

    /// Remove a user's password, leaving the account unable to authenticate by password until a new one
    /// is enrolled - the recovery path when a local password is lost.
    ///
    /// # Errors
    /// Returns a missing-user or storage error.
    pub fn clear_password(&self, id: &UserId) -> Result<(), UserStoreError> {
        self.store.clear_user_password(id)
    }

    /// An unknown name, a disabled account, a passwordless account, and a wrong password all fail the
    /// same way - `Ok(None)` after spending one derivation's worth of work - so none is distinguishable
    /// from the others by its response or its timing. A successful check whose verifier has fallen
    /// behind the policy re-enrolls it under the same ID before returning, and a failure to do so does
    /// not deny the login that already succeeded.
    ///
    /// # Errors
    /// Returns the storage error when the identity lookup itself fails, which denies the login.
    pub async fn authenticate(&self, display_name: &str, password: &str) -> Result<Option<UserId>, MetaError> {
        let active = match self.store.get_user_by_name(display_name) {
            Ok(user) => user.filter(|user| user.state == UserState::Active),
            Err(UserStoreError::Store(error)) => return Err(error),
            Err(_) => None,
        };
        let Some(user) = active else {
            self.spend_decoy(password.to_owned()).await;
            return Ok(None);
        };
        let Some(verifier) = self.store.get_user_password(&user.id)? else {
            self.spend_decoy(password.to_owned()).await;
            return Ok(None);
        };
        let (policy, presented) = (self.policy, password.to_owned());
        match self.gated(move || verifier.check(&presented, &policy)).await {
            PasswordCheck::Rejected => Ok(None),
            PasswordCheck::Accepted { stale } => {
                if stale {
                    let _ = self.set_password(&user.id, password).await;
                }
                Ok(Some(user.id))
            }
        }
    }

    async fn hash(&self, password: String) -> Result<PasswordVerifier, PasswordError> {
        let policy = self.policy;
        self.gated(move || policy.hash(&password)).await
    }

    async fn spend_decoy(&self, password: String) {
        let policy = self.policy;
        self.gated(move || policy.spend_decoy(&password)).await;
    }

    // Password derivation must not consume async request workers.
    async fn gated<T: Send + 'static>(&self, work: impl FnOnce() -> T + Send + 'static) -> T {
        let permit = Arc::clone(&self.verifications)
            .acquire_owned()
            .await
            .expect("the verification semaphore is never closed");
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            work()
        })
        .await
        .expect("the derivation task is never aborted")
    }
}
