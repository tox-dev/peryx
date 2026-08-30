use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::{BasicCredentials, parse_basic, secrets_match};

/// # Errors
/// Returns [`Denial::Unavailable`] when the index grants the action to no token,
/// [`Denial::Unauthenticated`] when the request lacks a required credential, or
/// [`Denial::Forbidden`] when the principal lacks a matching grant.
pub fn authorize(
    principal: &Principal,
    acl: &IndexAcl,
    resource: ResourceMatch<'_>,
    action: Action,
) -> Result<(), Denial> {
    if action == Action::Read && acl.anonymous_read {
        return Ok(());
    }
    let now = system_now();
    match principal {
        Principal::Named { subject } => acl
            .token(subject)
            .ok_or(Denial::Forbidden)
            .and_then(|token| authorize_grants(&token.grants, resource, action)),
        Principal::Anonymous if acl.grants_to_anyone_at(action, now) => Err(Denial::Unauthenticated),
        Principal::Anonymous => Err(Denial::Unavailable),
    }
}

/// Catalogs may gain resources; an explicit `*` grant authorizes the full catalog.
///
/// # Errors
/// Returns the same denial classes as [`authorize`].
pub fn authorize_all(principal: &Principal, acl: &IndexAcl, action: Action) -> Result<(), Denial> {
    if action == Action::Read && acl.anonymous_read {
        return Ok(());
    }
    match principal {
        Principal::Named { subject } => authorize_named_all(subject, acl, action),
        Principal::Anonymous if acl.grants_to_anyone_at(action, system_now()) => Err(Denial::Unauthenticated),
        Principal::Anonymous => Err(Denial::Unavailable),
    }
}

/// Authorizes an authenticated token over the full catalog without consulting `anonymous_read`.
///
/// Anonymous readability is an artifact-serving policy; a caller that presents a credential is
/// judged on that credential's grants alone.
///
/// # Errors
/// Returns [`Denial::Forbidden`] when the index holds no such token or none of its grants covers the
/// full catalog for `action`.
pub fn authorize_named_all(subject: &str, acl: &IndexAcl, action: Action) -> Result<(), Denial> {
    acl.token(subject)
        .ok_or(Denial::Forbidden)?
        .grants
        .iter()
        .any(|grant| grant.allows_all(action))
        .then_some(())
        .ok_or(Denial::Forbidden)
}

/// # Errors
/// Returns [`Denial::Forbidden`] when no grant covers the resource and action.
pub fn authorize_grants(grants: &[Grant], resource: ResourceMatch<'_>, action: Action) -> Result<(), Denial> {
    grants
        .iter()
        .any(|grant| grant.allows(resource, action))
        .then_some(())
        .ok_or(Denial::Forbidden)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceMatch<'a> {
    Any,
    Pattern(&'a str),
    Exact(&'a str),
}

/// A credential that matches no token leaves the request anonymous, with the same authority as no
/// credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    Anonymous,
    Named { subject: String },
}

/// Protocols map pulls to reads, pushes to writes, and removals to deletes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Read,
    Write,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Denial {
    /// No token grants this action to anyone on this index.
    Unavailable,
    /// The request carried no credential the action accepts.
    Unauthenticated,
    /// The principal is known and lacks a matching grant.
    Forbidden,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexAcl {
    /// Defaults to `true` to preserve pre-ACL read behavior.
    pub anonymous_read: bool,
    pub tokens: Vec<NamedToken>,
}

impl Default for IndexAcl {
    fn default() -> Self {
        Self {
            anonymous_read: true,
            tokens: Vec::new(),
        }
    }
}

impl IndexAcl {
    /// Resolves an `Authorization` header at `now`, in Unix seconds. Missing or malformed headers and
    /// passwords that match no live token yield [`Principal::Anonymous`].
    #[must_use]
    pub fn identify(&self, header: Option<&str>, now: i64) -> Identity {
        self.identify_credentials(header.and_then(parse_basic).as_ref(), now)
    }

    /// Resolves credentials the caller already parsed, so a resolver that classifies the
    /// `Authorization` header once does not decode it a second time.
    #[must_use]
    pub fn identify_credentials(&self, credentials: Option<&BasicCredentials>, now: i64) -> Identity {
        let Some(credentials) = credentials else {
            return Identity {
                principal: Principal::Anonymous,
                user: None,
            };
        };
        let principal = self
            .tokens
            .iter()
            .find(|token| token.live(now) && secrets_match(&credentials.password, &token.secret))
            .map_or(Principal::Anonymous, |token| Principal::Named {
                subject: token.name.clone(),
            });
        Identity {
            principal,
            user: Some(credentials.user.clone()),
        }
    }

    /// Anonymous reads come from `anonymous_read`, so anonymous principals have no grants.
    #[must_use]
    pub fn grants(&self, principal: &Principal) -> &[Grant] {
        match principal {
            Principal::Anonymous => &[],
            Principal::Named { subject } => self.token(subject).map_or(&[], |token| token.grants.as_slice()),
        }
    }

    #[must_use]
    pub fn grants_to_anyone(&self, action: Action) -> bool {
        self.tokens
            .iter()
            .any(|token| token.grants.iter().any(|grant| grant.actions.contains(&action)))
    }

    #[must_use]
    pub fn grants_to_anyone_at(&self, action: Action, now: i64) -> bool {
        self.tokens
            .iter()
            .any(|token| token.live(now) && token.grants.iter().any(|grant| grant.actions.contains(&action)))
    }

    fn token(&self, name: &str) -> Option<&NamedToken> {
        self.tokens.iter().find(|token| token.name == name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub principal: Principal,
    /// Audit context; [`authorize`] does not trust an unverified username.
    pub user: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedToken {
    pub name: String,
    pub secret: String,
    pub grants: Vec<Grant>,
    /// Unix seconds after which the token stops authenticating; `None` has no expiry.
    pub expires_at: Option<i64>,
}

impl NamedToken {
    fn live(&self, now: i64) -> bool {
        self.expires_at.is_none_or(|expiry| now < expiry)
    }
}

fn system_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub resources: Vec<Glob>,
    pub actions: BTreeSet<Action>,
}

impl Grant {
    fn allows(&self, resource: ResourceMatch<'_>, action: Action) -> bool {
        self.actions.contains(&action)
            && match resource {
                ResourceMatch::Any => true,
                ResourceMatch::Pattern(resource) => self.resources.iter().any(|pattern| pattern.matches(resource)),
                ResourceMatch::Exact(resource) => self.resources.iter().any(|pattern| pattern.as_str() == resource),
            }
    }

    fn allows_all(&self, action: Action) -> bool {
        self.actions.contains(&action) && self.resources.iter().any(|pattern| pattern.as_str() == "*")
    }
}

/// `*` stands for any run of characters, `/` included, so `team/*` covers every repository under
/// `team`, at any depth. `*` covers the whole index; all other characters match themselves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Glob(String);

impl Glob {
    #[must_use]
    pub fn new(pattern: impl Into<String>) -> Self {
        Self(pattern.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn matches_prefix(&self, prefix: &str) -> bool {
        self.remainders_after(prefix).next().is_some()
    }

    /// Returns each suffix that can match after consuming `prefix`.
    pub fn remainders_after<'a>(&'a self, prefix: &str) -> impl Iterator<Item = &'a str> {
        let pattern = self.0.as_bytes();
        let mut reachable = vec![false; pattern.len() + 1];
        let mut next = vec![false; pattern.len() + 1];
        reachable[0] = true;
        close_stars(pattern, &mut reachable);
        for byte in prefix.bytes() {
            next.fill(false);
            for (position, &active) in reachable[..pattern.len()].iter().enumerate() {
                if active {
                    if pattern[position] == b'*' {
                        next[position] = true;
                    } else if pattern[position] == byte {
                        next[position + 1] = true;
                    }
                }
            }
            close_stars(pattern, &mut next);
            std::mem::swap(&mut reachable, &mut next);
        }
        reachable
            .into_iter()
            .enumerate()
            .filter(|(_, active)| *active)
            .map(|(position, _)| &self.0[position..])
    }

    #[must_use]
    pub fn matches(&self, resource: &str) -> bool {
        self.remainders_after(resource)
            .any(|remainder| remainder.bytes().all(|byte| byte == b'*'))
    }
}

fn close_stars(pattern: &[u8], reachable: &mut [bool]) {
    for position in 0..pattern.len() {
        if reachable[position] && pattern[position] == b'*' {
            reachable[position + 1] = true;
        }
    }
}
