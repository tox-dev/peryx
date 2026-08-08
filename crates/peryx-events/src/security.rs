//! Structured security events for index actions and server-role decisions.

use http::{HeaderMap, header};
use peryx_identity::{Identity, Principal, Role, Scope, UserId};

const UNKNOWN: &str = "unknown";

/// Whether a role-grant mutation added or removed a binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleGrantChange {
    Grant,
    Revoke,
}

impl RoleGrantChange {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Grant => "grant",
            Self::Revoke => "revoke",
        }
    }
}

/// Record a role-grant mutation attempt with only bounded identifiers.
///
/// The event names the granting actor, the target user, the role, the reach, and the resolved outcome.
/// It carries no request body and no secret, so an audit log keeps the who, what, and result of every
/// delegation without disclosing more.
pub fn role_grant_change(
    actor: Option<&str>,
    change: RoleGrantChange,
    target: &UserId,
    role: Role,
    reach: &str,
    result: &'static str,
    reason: &'static str,
) {
    let actor = text(actor);
    let action = change.as_str();
    let target = target.as_str();
    let role = role.as_str();
    tracing::info!(
        target: "peryx::security",
        security_event = true,
        event = "role_grant",
        action,
        actor,
        target,
        role,
        reach,
        result,
        reason,
        "role grant mutation"
    );
}

/// A bounded reason for denying a role-based authorization decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizationDenial {
    NoGrant,
    StorageUnavailable,
}

impl AuthorizationDenial {
    const fn as_str(self) -> &'static str {
        match self {
            Self::NoGrant => "no_grant",
            Self::StorageUnavailable => "storage_unavailable",
        }
    }
}

/// Record a denied role authorization without accepting a resource path or query string.
pub fn authorization_denied(user: &UserId, scope: Scope, denial: AuthorizationDenial) {
    let user = user.as_str();
    let scope = scope.as_str();
    let reason = denial.as_str();
    tracing::info!(
        target: "peryx::security",
        security_event = true,
        event = "authorization",
        user,
        scope,
        result = "denied",
        reason,
        "role authorization denied"
    );
}

pub struct Event<'a> {
    action: &'static str,
    result: &'static str,
    actor: Option<&'a str>,
    publisher_id: Option<&'a str>,
    token_id: Option<&'a str>,
    index: Option<&'a str>,
    source_index: Option<&'a str>,
    hosted_index: Option<&'a str>,
    project: Option<&'a str>,
    version: Option<&'a str>,
    filename: Option<&'a str>,
    digest: Option<&'a str>,
    count: usize,
    changed: bool,
    reason: Option<&'a str>,
    request_id: Option<&'a str>,
    user_agent: Option<&'a str>,
}

impl<'a> Event<'a> {
    #[must_use]
    pub const fn new(action: &'static str, result: &'static str) -> Self {
        Self {
            action,
            result,
            actor: None,
            publisher_id: None,
            token_id: None,
            index: None,
            source_index: None,
            hosted_index: None,
            project: None,
            version: None,
            filename: None,
            digest: None,
            count: 0,
            changed: false,
            reason: None,
            request_id: None,
            user_agent: None,
        }
    }

    #[must_use]
    pub const fn actor(mut self, actor: Option<&'a str>) -> Self {
        self.actor = actor;
        self
    }

    #[must_use]
    pub const fn publisher_id(mut self, publisher_id: &'a str) -> Self {
        self.publisher_id = Some(publisher_id);
        self
    }

    #[must_use]
    pub const fn token_id(mut self, token_id: &'a str) -> Self {
        self.token_id = Some(token_id);
        self
    }

    #[must_use]
    pub const fn index(mut self, index: &'a str) -> Self {
        self.index = Some(index);
        self
    }

    #[must_use]
    pub const fn source_index(mut self, source_index: &'a str) -> Self {
        self.source_index = Some(source_index);
        self
    }

    #[must_use]
    pub const fn hosted_index(mut self, hosted_index: &'a str) -> Self {
        self.hosted_index = Some(hosted_index);
        self
    }

    #[must_use]
    pub const fn project(mut self, project: Option<&'a str>) -> Self {
        self.project = project;
        self
    }

    #[must_use]
    pub const fn version(mut self, version: Option<&'a str>) -> Self {
        self.version = version;
        self
    }

    #[must_use]
    pub const fn filename(mut self, filename: Option<&'a str>) -> Self {
        self.filename = filename;
        self
    }

    #[must_use]
    pub const fn digest(mut self, digest: Option<&'a str>) -> Self {
        self.digest = digest;
        self
    }

    #[must_use]
    pub const fn count(mut self, count: usize) -> Self {
        self.count = count;
        self
    }

    #[must_use]
    pub const fn changed(mut self, changed: bool) -> Self {
        self.changed = changed;
        self
    }

    #[must_use]
    pub const fn reason(mut self, reason: Option<&'a str>) -> Self {
        self.reason = reason;
        self
    }

    #[must_use]
    pub fn request(mut self, headers: &'a HeaderMap) -> Self {
        self.request_id = request_id(headers);
        self.user_agent = user_agent(headers);
        self
    }

    pub fn emit(&self) {
        let actor = text(self.actor);
        let publisher_id = text(self.publisher_id);
        let token_id = text(self.token_id);
        let index = text(self.index);
        let source_index = text(self.source_index);
        let hosted_index = text(self.hosted_index);
        let project = text(self.project);
        let version = text(self.version);
        let filename = text(self.filename);
        let digest = text(self.digest);
        let reason = text(self.reason);
        let request_id = text(self.request_id);
        let user_agent = text(self.user_agent);
        tracing::info!(
            target: "peryx::security",
            security_event = true,
            event = "index_action",
            action = self.action,
            result = self.result,
            actor,
            publisher_id,
            token_id,
            index,
            source_index,
            hosted_index,
            project,
            version,
            filename,
            digest,
            count = self.count,
            changed = self.changed,
            reason,
            request_id,
            user_agent,
            "index security event"
        );
    }
}

/// Who to record an action against, from the identity the index's ACL already resolved.
///
/// The username a Basic credential carried names the actor whether or not it authenticated, so a
/// refused push still records who tried. A bearer credential carries no username; there the actor is
/// the principal the token names.
#[must_use]
pub fn actor(identity: &Identity) -> Option<String> {
    if let Some(user) = &identity.user {
        return Some(if user.is_empty() {
            UNKNOWN.to_owned()
        } else {
            user.clone()
        });
    }
    match &identity.principal {
        Principal::Named { subject } => Some(subject.clone()),
        Principal::Anonymous => None,
    }
}

fn request_id(headers: &HeaderMap) -> Option<&str> {
    header_str(headers, "x-request-id")
}

fn user_agent(headers: &HeaderMap) -> Option<&str> {
    header_str(headers, header::USER_AGENT.as_str())
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

fn text(value: Option<&str>) -> &str {
    value.unwrap_or("")
}

#[cfg(test)]
#[path = "../tests/unit/security/tests.rs"]
mod tests;
