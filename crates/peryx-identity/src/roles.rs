//! Persistent-user RBAC, separate from the per-index token ACL in [`crate::acl`]. Roles bind to
//! [`UserId`] accounts rather than resolved credentials.
//!
//! Authorization follows the deny-by-default [Kubernetes authorization] model: a grant must cover the
//! requested [`Scope`] and [`Resource`]. Four fixed roles follow [NIST RBAC]; the server persists the
//! user's role and [`GrantScope`] instead of a mutable permission set.
//!
//! [Kubernetes authorization]: https://kubernetes.io/docs/reference/access-authn-authz/authorization/
//! [NIST RBAC]: https://csrc.nist.gov/projects/role-based-access-control

use serde::{Deserialize, Serialize};

use crate::UserId;

/// Returns `false` for empty grant sets and grants that miss either the scope or resource.
#[must_use]
pub fn grants_permit(grants: &[RoleGrant], scope: Scope, resource: &Resource) -> bool {
    grants.iter().any(|grant| grant.permits(scope, resource))
}

/// Only an [`Administrator`](Role::Administrator) may delegate within its current reach.
/// This prevents privilege escalation under [Kubernetes RBAC].
///
/// [Kubernetes RBAC]: https://kubernetes.io/docs/reference/access-authn-authz/rbac/#privilege-escalation-prevention-and-bootstrapping
#[must_use]
pub fn can_manage_grants(caller: &[RoleGrant], reach: &GrantScope) -> bool {
    caller
        .iter()
        .any(|held| held.role == Role::Administrator && held.scope.covers_reach(reach))
}

/// Storage persists bindings; [`Scope`] and [`Resource`] remain decision inputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleGrant {
    pub user: UserId,
    pub role: Role,
    pub scope: GrantScope,
}

impl RoleGrant {
    #[must_use]
    pub const fn new(user: UserId, role: Role, scope: GrantScope) -> Self {
        Self { user, role, scope }
    }

    /// The role must carry `scope`, the resource class must match, and the grant must cover `resource`.
    #[must_use]
    pub fn permits(&self, scope: Scope, resource: &Resource) -> bool {
        self.role.carries(scope) && scope.applies_to(resource) && self.scope.covers(resource)
    }

    /// Repository bindings require a repository scope; an `Operator` repository binding has no
    /// authority and must not reach storage.
    #[must_use]
    pub fn is_effective(&self) -> bool {
        match &self.scope {
            GrantScope::Server => true,
            GrantScope::Repository { name } => {
                let resource = Resource::Repository(name.clone());
                self.role.scopes().iter().any(|scope| scope.applies_to(&resource))
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// All repository, operator, analytics, and administration scopes.
    Administrator,
    /// Read, write, and delete on the granted repository.
    RepositoryPublisher,
    /// Read on the granted repository.
    RepositoryReader,
    /// Read operator and analytics data without repository access.
    Operator,
}

impl Role {
    /// Lists roles in the stable order used by help text and the UI.
    pub const ALL: &'static [Self] = &[
        Self::Administrator,
        Self::RepositoryPublisher,
        Self::RepositoryReader,
        Self::Operator,
    ];

    /// Stable identifier for configuration, APIs, the UI, and security events.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Administrator => "administrator",
            Self::RepositoryPublisher => "repository_publisher",
            Self::RepositoryReader => "repository_reader",
            Self::Operator => "operator",
        }
    }

    /// Repository roles omit server-data scopes, preventing repository grants from exposing operator
    /// data.
    #[must_use]
    pub const fn scopes(self) -> &'static [Scope] {
        match self {
            Self::Administrator => &[
                Scope::RepositoryRead,
                Scope::RepositoryWrite,
                Scope::RepositoryDelete,
                Scope::OperatorRead,
                Scope::AnalyticsRead,
                Scope::AdministrationRead,
                Scope::AdministrationWrite,
            ],
            Self::RepositoryPublisher => &[Scope::RepositoryRead, Scope::RepositoryWrite, Scope::RepositoryDelete],
            Self::RepositoryReader => &[Scope::RepositoryRead],
            Self::Operator => &[Scope::OperatorRead, Scope::AnalyticsRead],
        }
    }

    fn carries(self, scope: Scope) -> bool {
        self.scopes().contains(&scope)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    RepositoryRead,
    RepositoryWrite,
    RepositoryDelete,
    OperatorRead,
    AnalyticsRead,
    AdministrationRead,
    AdministrationWrite,
}

impl Scope {
    /// The colon namespace prevents collisions between resource classes in security events.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RepositoryRead => "repository:read",
            Self::RepositoryWrite => "repository:write",
            Self::RepositoryDelete => "repository:delete",
            Self::OperatorRead => "operator:read",
            Self::AnalyticsRead => "analytics:read",
            Self::AdministrationRead => "administration:read",
            Self::AdministrationWrite => "administration:write",
        }
    }

    const fn applies_to(self, resource: &Resource) -> bool {
        matches!(
            (self, resource),
            (
                Self::RepositoryRead | Self::RepositoryWrite | Self::RepositoryDelete,
                Resource::Repository(_)
            ) | (
                Self::OperatorRead | Self::AnalyticsRead | Self::AdministrationRead | Self::AdministrationWrite,
                Resource::Operator
            )
        )
    }
}

/// Repository grants cannot reach other repositories or operator data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GrantScope {
    Server,
    Repository { name: String },
}

impl GrantScope {
    fn covers(&self, resource: &Resource) -> bool {
        match (self, resource) {
            (Self::Server, _) => true,
            (Self::Repository { name }, Resource::Repository(target)) => name == target,
            (Self::Repository { .. }, Resource::Operator) => false,
        }
    }

    fn covers_reach(&self, target: &Self) -> bool {
        match (self, target) {
            (Self::Server, _) => true,
            (Self::Repository { name }, Self::Repository { name: target }) => name == target,
            (Self::Repository { .. }, Self::Server) => false,
        }
    }
}

/// Repository scopes target a named repository; server-data scopes target [`Resource::Operator`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resource {
    Repository(String),
    Operator,
}

impl Resource {
    /// Fields recorded for an allowed security event.
    #[must_use]
    pub fn fields(&self) -> (&'static str, &str) {
        match self {
            Self::Repository(name) => ("repository", name),
            Self::Operator => ("operator", ""),
        }
    }
}
