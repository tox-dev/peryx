//! Authorization combines persisted role grants ([`MetaStore`]) with the fixed
//! [`peryx_identity::grants_permit`] decision model.
//!
//! Each decision reads authoritative grants. Revocations take effect on the next decision without
//! cache invalidation. Snapshot reads do not write to the database.
//!
//! Decisions fail closed. When the grant store cannot be read the answer is [`Decision::Deny`] with
//! [`DenyReason::StorageUnavailable`], never an allow, so a storage fault cannot open access. Each
//! decision emits one bounded security event. Allowed events carry the resource; denied events omit it
//! so a failed check cannot disclose a protected path or query.

use peryx_events::security::{AuthorizationDenial, authorization_denied};
use peryx_identity::{GrantScope, Resource, Role, RoleGrant, Scope, UserId, grants_permit};
use peryx_storage::meta::{CreateGrantOutcome, MetaError, MetaStore, RoleGrantPage};
pub use peryx_storage::meta::{
    DeleteGrantOutcome, RoleGrantFilter, RoleGrantQuery, RoleGrantQueryError, RoleGrantStoreError, StoredRoleGrant,
    role_grant_reach,
};

/// Role-based authorization over persistent server users.
#[derive(Debug, Clone)]
pub struct AuthorizationService {
    store: MetaStore,
}

/// The outcome of an authorization decision. It has no allow variant that carries a storage error, so
/// a fault can only ever deny.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny(DenyReason),
}

/// Why a decision denied: the user held no covering grant, or the grants could not be read and the
/// decision failed closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    NoGrant,
    StorageUnavailable,
}

/// A decision bound to the scope the authorization service checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScopedDecision {
    scope: Scope,
    decision: Decision,
}

impl ScopedDecision {
    #[must_use]
    pub const fn scope(self) -> Scope {
        self.scope
    }

    #[must_use]
    pub const fn decision(self) -> Decision {
        self.decision
    }
}

impl Decision {
    #[must_use]
    pub const fn is_allowed(self) -> bool {
        matches!(self, Self::Allow)
    }
}

impl AuthorizationService {
    #[must_use]
    pub const fn new(store: MetaStore) -> Self {
        Self { store }
    }

    /// # Errors
    /// Returns [`RoleGrantStoreError::UnknownUser`] for an unknown user or a store error when the
    /// grant cannot be committed.
    pub fn grant(&self, user: &UserId, role: Role, scope: GrantScope) -> Result<RoleGrant, RoleGrantStoreError> {
        self.store.grant_role(user, role, scope)
    }

    /// # Errors
    /// Returns a store error when the revocation cannot be committed.
    pub fn revoke(&self, user: &UserId, role: Role, scope: &GrantScope) -> Result<bool, MetaError> {
        self.store.revoke_role(user, role, scope)
    }

    /// # Errors
    /// Returns a store error when the grants cannot be read.
    pub fn grants(&self, user: &UserId) -> Result<Vec<RoleGrant>, MetaError> {
        self.store.user_role_grants(user)
    }

    /// # Errors
    /// Returns [`RoleGrantStoreError`] for a missing or disabled user, or a store fault.
    pub fn create_managed_grant(
        &self,
        grant: &RoleGrant,
        granted_by: &UserId,
        now: i64,
    ) -> Result<CreateGrantOutcome, RoleGrantStoreError> {
        self.store.create_managed_grant(grant, granted_by, now)
    }

    /// # Errors
    /// Returns a store error when the record cannot be read or decoded.
    pub fn managed_grant(&self, id: &str) -> Result<Option<StoredRoleGrant>, MetaError> {
        self.store.managed_grant(id)
    }

    /// # Errors
    /// Returns a store error when the transaction cannot commit.
    pub fn delete_managed_grant(&self, id: &str, expected_version: u64) -> Result<DeleteGrantOutcome, MetaError> {
        self.store.delete_managed_grant(id, expected_version)
    }

    /// # Errors
    /// Returns [`RoleGrantQueryError`] for an out-of-range limit or a store fault.
    pub fn list_managed_grants(&self, query: &RoleGrantQuery) -> Result<RoleGrantPage, RoleGrantQueryError> {
        self.store.list_managed_grants(query)
    }

    #[must_use]
    pub fn authorize(&self, user: &UserId, scope: Scope, resource: &Resource) -> Decision {
        let decision = match self.store.user_role_grants(user) {
            Ok(grants) if grants_permit(&grants, scope, resource) => Decision::Allow,
            Ok(_) => Decision::Deny(DenyReason::NoGrant),
            Err(_) => Decision::Deny(DenyReason::StorageUnavailable),
        };
        // Compute the log fields before the macro: as macro arguments they would evaluate only when the
        // callsite is enabled, so a run without a security-log subscriber would never cover them.
        if let Decision::Deny(reason) = decision {
            authorization_denied(
                user,
                scope,
                match reason {
                    DenyReason::NoGrant => AuthorizationDenial::NoGrant,
                    DenyReason::StorageUnavailable => AuthorizationDenial::StorageUnavailable,
                },
            );
            return decision;
        }
        let user = user.as_str();
        let scope = scope.as_str();
        let (resource_kind, resource_name) = resource.fields();
        tracing::info!(
            target: "peryx::security",
            security_event = true,
            event = "authorization",
            user,
            scope,
            resource_kind,
            resource = resource_name,
            result = "allowed",
            reason = "granted",
            "role authorization decision"
        );
        decision
    }

    #[must_use]
    pub fn authorize_scoped(&self, user: &UserId, scope: Scope, resource: &Resource) -> ScopedDecision {
        ScopedDecision {
            scope,
            decision: self.authorize(user, scope, resource),
        }
    }
}
