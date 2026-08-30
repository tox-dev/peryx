use std::sync::Arc;

use peryx_identity::{
    PasswordCheck, PasswordError, PasswordPolicy, PasswordVerifier, ServerUser, UserId, UserLifecycleEvent, UserState,
};
use peryx_storage::meta::{AdministratorBootstrapError, MetaError, MetaStore, UserStoreError};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// How many password derivations may run at once by default, chosen well under the request worker
/// count so a burst of logins cannot starve request serving.
const DEFAULT_PASSWORD_CHECKS: usize = 4;

/// Persistent users and bounded local-password authentication.
#[derive(Debug, Clone)]
pub struct UserService {
    store: MetaStore,
    policy: PasswordPolicy,
    password_admission: Arc<Semaphore>,
    password_workers: Arc<Semaphore>,
}

struct PasswordAdmission {
    permit: Arc<OwnedSemaphorePermit>,
}

/// A password derivation that could not complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PasswordDerivationError {
    #[error("password derivation capacity exhausted")]
    Overloaded,
    #[error(transparent)]
    Hash(#[from] PasswordError),
}

/// A rejected password enrollment.
#[derive(Debug, thiserror::Error)]
pub enum EnrollError {
    #[error(transparent)]
    Derivation(#[from] PasswordDerivationError),
    #[error(transparent)]
    Store(#[from] UserStoreError),
}

impl From<PasswordError> for EnrollError {
    fn from(error: PasswordError) -> Self {
        Self::Derivation(error.into())
    }
}

/// A rejected first-administrator bootstrap.
#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error(transparent)]
    Derivation(#[from] PasswordDerivationError),
    #[error(transparent)]
    Store(#[from] AdministratorBootstrapError),
}

impl From<PasswordError> for BootstrapError {
    fn from(error: PasswordError) -> Self {
        Self::Derivation(error.into())
    }
}

/// A password authentication failure that is not a rejected credential.
#[derive(Debug, thiserror::Error)]
pub enum AuthenticationError {
    #[error(transparent)]
    Derivation(#[from] PasswordDerivationError),
    #[error(transparent)]
    Store(#[from] MetaError),
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
            // One waiting batch absorbs scheduling jitter without retaining an open-ended backlog.
            password_admission: Arc::new(Semaphore::new(
                max_concurrent_checks.saturating_mul(2).min(Semaphore::MAX_PERMITS),
            )),
            password_workers: Arc::new(Semaphore::new(max_concurrent_checks)),
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
    /// Returns [`EnrollError::Derivation`] when derivation fails or the password queue is full, and
    /// [`EnrollError::Store`] for an unknown user or a storage failure.
    pub async fn set_password(&self, id: &UserId, password: &str) -> Result<(), EnrollError> {
        let admission = self.admit()?;
        let verifier = self.hash(&admission, password.to_owned()).await?;
        drop(admission);
        self.store.set_user_password(id, &verifier)?;
        Ok(())
    }

    /// # Errors
    /// Returns [`BootstrapError::Derivation`] when derivation fails or the password queue is full, and
    /// [`BootstrapError::Store`] when an administrator exists, the identity conflicts, or the metadata
    /// transaction aborts.
    pub async fn bootstrap_administrator(
        &self,
        display_name: &str,
        password: &str,
    ) -> Result<ServerUser, BootstrapError> {
        let admission = self.admit()?;
        let verifier = self.hash(&admission, password.to_owned()).await?;
        drop(admission);
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
    /// behind the policy re-enrolls it under the same ID before returning. The store replaces only the
    /// verifier checked; authentication fails if another request changed it.
    ///
    /// # Errors
    /// Returns an unavailable error when lookup, derivation admission, hashing, or conditional
    /// replacement fails.
    pub async fn authenticate(
        &self,
        display_name: &str,
        password: &str,
    ) -> Result<Option<UserId>, AuthenticationError> {
        let admission = self.admit()?;
        let active = match self.store.get_user_by_name(display_name) {
            Ok(user) => user.filter(|user| user.state == UserState::Active),
            Err(UserStoreError::Store(error)) => return Err(error.into()),
            Err(_) => None,
        };
        let Some(user) = active else {
            self.spend_decoy(&admission, password.to_owned()).await;
            drop(admission);
            return Ok(None);
        };
        let Some(verifier) = self.store.get_user_password(&user.id)? else {
            self.spend_decoy(&admission, password.to_owned()).await;
            drop(admission);
            return Ok(None);
        };
        tracing::trace!(target: "peryx_driver::users::password_verifier_read", user_id = %user.id);
        let (policy, presented, checked) = (self.policy, password.to_owned(), verifier.verifier().clone());
        match self.run(&admission, move || checked.check(&presented, &policy)).await {
            PasswordCheck::Rejected => {
                drop(admission);
                Ok(None)
            }
            PasswordCheck::Accepted { stale: false } => {
                drop(admission);
                Ok(Some(user.id))
            }
            PasswordCheck::Accepted { stale: true } => {
                let replacement = self.hash(&admission, password.to_owned()).await?;
                drop(admission);
                let replaced = self
                    .store
                    .compare_and_set_user_password(&user.id, &verifier, &replacement)?;
                Ok(replaced.then_some(user.id))
            }
        }
    }

    fn admit(&self) -> Result<PasswordAdmission, PasswordDerivationError> {
        let admission = Arc::clone(&self.password_admission)
            .try_acquire_owned()
            .map_err(|_| PasswordDerivationError::Overloaded)?;
        tracing::trace!(target: "peryx_driver::users::password_derivation_admitted", "admitted");
        Ok(PasswordAdmission {
            permit: Arc::new(admission),
        })
    }

    async fn hash(
        &self,
        admission: &PasswordAdmission,
        password: String,
    ) -> Result<PasswordVerifier, PasswordDerivationError> {
        let policy = self.policy;
        self.run(admission, move || policy.hash(&password))
            .await
            .map_err(Into::into)
    }

    async fn spend_decoy(&self, admission: &PasswordAdmission, password: String) {
        let policy = self.policy;
        self.run(admission, move || policy.spend_decoy(&password)).await;
    }

    // Password derivation must not consume async request workers.
    async fn run<T: Send + 'static>(
        &self,
        admission: &PasswordAdmission,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> T {
        let admission = Arc::clone(&admission.permit);
        let worker = Arc::clone(&self.password_workers)
            .acquire_owned()
            .await
            .expect("the password worker semaphore is never closed");
        tokio::task::spawn_blocking(move || {
            let (_admission, _worker) = (admission, worker);
            tracing::trace!(target: "peryx_driver::users::password_derivation_started", "started");
            work()
        })
        .await
        .expect("the derivation task is never aborted")
    }
}
