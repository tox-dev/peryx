use std::net::IpAddr;

use http::{HeaderMap, header};
use peryx_identity::{Identity, Principal, Role, Scope, UserId};

const UNKNOWN: &str = "unknown";

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

/// Records bounded identifiers without request bodies or secrets.
pub fn role_grant_change(
    actor: Option<&str>,
    change: RoleGrantChange,
    target: &UserId,
    role: Role,
    reach: &str,
    result: &'static str,
    reason: &'static str,
) {
    let action = change.as_str();
    let actor = text(actor);
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

/// Excludes unbounded resource paths and query strings.
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

/// Request headers and the client address established by HTTP middleware.
#[derive(Clone, Copy)]
pub struct RequestContext<'a> {
    pub headers: &'a HeaderMap,
    pub client_ip: Option<IpAddr>,
}

impl<'a> RequestContext<'a> {
    #[must_use]
    pub const fn new(headers: &'a HeaderMap, client_ip: Option<IpAddr>) -> Self {
        Self { headers, client_ip }
    }
}

pub struct Event<'a> {
    action: &'static str,
    result: &'static str,
    actor: Option<&'a str>,
    token_id: Option<&'a str>,
    index: Option<&'a str>,
    source_index: Option<&'a str>,
    hosted_index: Option<&'a str>,
    resource: Option<&'a str>,
    group: Option<&'a str>,
    artifact: Option<&'a str>,
    digest: Option<&'a str>,
    count: usize,
    changed: bool,
    reason: Option<&'a str>,
    request_id: Option<&'a str>,
    user_agent: Option<&'a str>,
    client_ip: Option<IpAddr>,
}

impl<'a> Event<'a> {
    #[must_use]
    pub const fn new(action: &'static str, result: &'static str) -> Self {
        Self {
            action,
            result,
            actor: None,
            token_id: None,
            index: None,
            source_index: None,
            hosted_index: None,
            resource: None,
            group: None,
            artifact: None,
            digest: None,
            count: 0,
            changed: false,
            reason: None,
            request_id: None,
            user_agent: None,
            client_ip: None,
        }
    }

    #[must_use]
    pub const fn actor(mut self, actor: Option<&'a str>) -> Self {
        self.actor = actor;
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
    pub const fn resource(mut self, resource: Option<&'a str>) -> Self {
        self.resource = resource;
        self
    }

    #[must_use]
    pub const fn group(mut self, group: Option<&'a str>) -> Self {
        self.group = group;
        self
    }

    #[must_use]
    pub const fn artifact(mut self, artifact: Option<&'a str>) -> Self {
        self.artifact = artifact;
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
    pub fn request(mut self, request: RequestContext<'a>) -> Self {
        self.request_id = request_id(request.headers);
        self.user_agent = user_agent(request.headers);
        self.client_ip = request.client_ip;
        self
    }

    pub fn emit(&self) {
        let actor = text(self.actor);
        let token_id = text(self.token_id);
        let index = text(self.index);
        let source_index = text(self.source_index);
        let hosted_index = text(self.hosted_index);
        let resource = text(self.resource);
        let group = text(self.group);
        let artifact = text(self.artifact);
        let digest = text(self.digest);
        let reason = text(self.reason);
        let request_id = text(self.request_id);
        let user_agent = text(self.user_agent);
        let client_ip = self
            .client_ip
            .map(|client_ip| client_ip.to_string())
            .unwrap_or_default();
        let client_ip = client_ip.as_str();
        tracing::info!(
            target: "peryx::security",
            security_event = true,
            event = "index_action",
            action = self.action,
            result = self.result,
            actor,
            token_id,
            index,
            source_index,
            hosted_index,
            resource,
            group,
            artifact,
            digest,
            count = self.count,
            changed = self.changed,
            reason,
            request_id,
            user_agent,
            client_ip,
            "index security event"
        );
    }
}

/// Preserves a presented username so failed authentication remains attributable.
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
