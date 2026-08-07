//! Role-based access control, the decision model that sits beside the per-index token ACL.
//!
//! The token ACL in [`crate::acl`] answers "does this credential's grant cover this project?" through
//! its named tokens. This module answers a different question
//! that the server user model asks: "does this *user* hold a role that permits this scope on this
//! resource?" A [`Principal`](crate::Principal) is a resolved credential; a [`UserId`] is a persistent
//! account, and roles bind to the account.
//!
//! The model is deny-by-default after [Kubernetes authorization]: a decision starts denied and a grant
//! must affirmatively cover both the [`Scope`] and the [`Resource`] to allow it. Roles are fixed after
//! [NIST RBAC] - an operator grants a user one of four built-in roles rather than assembling scopes by
//! hand - so the scope set of a role is a constant this module owns, not persisted state that could
//! drift. Only the binding of a user to a role over a [`GrantScope`] is persisted.
//!
//! [Kubernetes authorization]: https://kubernetes.io/docs/reference/access-authn-authz/authorization/
//! [NIST RBAC]: https://csrc.nist.gov/projects/role-based-access-control

use serde::{Deserialize, Serialize};

use crate::UserId;

/// Whether any of a user's grants permits `scope` on `resource`. Deny-by-default: an empty slice, an
/// unknown user's (also empty) slice, and grants that miss on either axis all return `false`.
#[must_use]
pub fn grants_permit(grants: &[RoleGrant], scope: Scope, resource: &Resource) -> bool {
    grants.iter().any(|grant| grant.permits(scope, resource))
}

/// Whether a caller holding `caller` may administer grants over `reach`.
///
/// This is the delegation authority the grant API gates on: creating and removing other users' bindings
/// there. It stops a caller from widening its own reach (privilege escalation, after [Kubernetes RBAC]).
///
/// Only an [`Administrator`](Role::Administrator) delegates, and only within its reach: a server
/// administrator over any repository and over server data, a repository administrator over its own
/// repository and nothing else. A publisher or reader holds no delegation authority however broad its
/// data access, so managing who may act is never a side effect of being able to act. Because an
/// administrator's authority already covers every role weaker than its own over that reach, a delegated
/// binding can never exceed what the caller itself holds.
///
/// [Kubernetes RBAC]: https://kubernetes.io/docs/reference/access-authn-authz/rbac/#privilege-escalation-prevention-and-bootstrapping
#[must_use]
pub fn can_manage_grants(caller: &[RoleGrant], reach: &GrantScope) -> bool {
    caller
        .iter()
        .any(|held| held.role == Role::Administrator && held.scope.covers_reach(reach))
}

/// A persisted binding of a user to a role over a reach.
///
/// The stored authority a decision reads; [`Scope`] and [`Resource`] never persist, they are decision
/// inputs.
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

    /// Whether this grant permits `scope` on `resource`: the role must carry the scope, its resource
    /// class must match, and the reach must cover the resource. Any miss denies.
    #[must_use]
    pub fn permits(&self, scope: Scope, resource: &Resource) -> bool {
        self.role.carries(scope) && scope.applies_to(resource) && self.scope.covers(resource)
    }

    /// Whether this binding confers any authority. A server reach always does; a repository reach does
    /// only when the role carries a repository scope - so `Operator` over a repository, which reaches no
    /// operator data, is inert and rejected before it is ever stored.
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

/// A built-in role, the unit an operator grants.
///
/// The scope set of each is fixed here, so a grant persists only which role a user holds and never a
/// hand-assembled permission set that could drift from the four the server understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Every scope, including operator data: the role a server administrator holds.
    Administrator,
    /// Read, write, and delete on the granted repository, and nothing off it.
    RepositoryPublisher,
    /// Read on the granted repository, and nothing more.
    RepositoryReader,
    /// Read operator and analytics data, without any repository grant.
    Operator,
}

impl Role {
    /// Every role, in a stable order, for help text and the UI.
    pub const ALL: &'static [Self] = &[
        Self::Administrator,
        Self::RepositoryPublisher,
        Self::RepositoryReader,
        Self::Operator,
    ];

    /// The stable identifier used in config, the API, the UI, and security events.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Administrator => "administrator",
            Self::RepositoryPublisher => "repository_publisher",
            Self::RepositoryReader => "repository_reader",
            Self::Operator => "operator",
        }
    }

    /// The scopes this role carries. Administrator is the union; repository roles carry no server-data
    /// scope, which stops a publisher from reading operator data however it is granted.
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

/// A permission scope: the stable name of one thing a request may do. A role is a set of these, and a
/// decision names the one it requires.
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
    /// The stable identifier used in security events. The colon namespace keeps repository and operator
    /// scopes from colliding as the set grows.
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

/// The reach of a grant: the whole server, or one named repository.
///
/// A server grant covers every resource; a repository grant covers only its own repository and never
/// operator data, so a grant cannot widen a role's reach past the resource it names.
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

    /// Whether a grant over this reach also reaches everything `target` names: a server reach covers
    /// every reach, a repository reach only its own repository.
    fn covers_reach(&self, target: &Self) -> bool {
        match (self, target) {
            (Self::Server, _) => true,
            (Self::Repository { name }, Self::Repository { name: target }) => name == target,
            (Self::Repository { .. }, Self::Server) => false,
        }
    }
}

/// The thing a decision is taken against. A repository scope is checked against a named repository;
/// operator scopes are checked against the single server-wide [`Resource::Operator`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resource {
    Repository(String),
    Operator,
}

impl Resource {
    /// The resource class and name an allowed security event records.
    #[must_use]
    pub fn fields(&self) -> (&'static str, &str) {
        match self {
            Self::Repository(name) => ("repository", name),
            Self::Operator => ("operator", ""),
        }
    }
}
