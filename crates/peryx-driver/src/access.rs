//! Neutral HTTP surfaces share this resolver so presentation routes enforce index ACLs.
//!
//! [`ReadAccess::for_request`] adds the browser session to that resolution and is what protected
//! reads use; [`ReadAccess::from_headers`] stays credential-only for the client protocol routes,
//! which keeps a session cookie out of uploads and mutations.

use std::borrow::Cow;

use axum::http::{HeaderMap, header};
use peryx_identity::{
    Action, BasicCredentials, Denial, Grant, Principal, Resource, ResourceMatch, RoleGrant, SESSION_COOKIE, Scope,
    ServerUser, UserState, authorize, authorize_grants, grants_permit, parse_basic, strip_auth_scheme,
};
use peryx_search::{SearchAccess, SearchAccessPattern};

use crate::{Index, ServingState};

pub struct ReadAccess {
    credential: Credential,
}

enum Credential {
    Acl {
        credentials: Option<BasicCredentials>,
        now: i64,
    },
    Bearer(Vec<Grant>),
    /// The signed-in account's role grants as metadata holds them for this request.
    Session(Vec<RoleGrant>),
}

/// What a protected read's `Authorization` header carries, before any resource decision.
///
/// Only [`Self::Absent`] may fall back to an approved browser session: a credential the request
/// carries decides that request, and an invalid one stays terminal instead of reaching a second
/// access path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderCredential {
    Absent,
    Verified(VerifiedCredential),
    Invalid(InvalidCredential),
}

/// A credential the realm recognized. A `Basic` pair still authenticates against the index ACL's
/// token secrets or the user store; a bearer token arrives with its signature already checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifiedCredential {
    Basic(BasicCredentials),
    Bearer(Vec<Grant>),
}

/// Why a credential can authorize nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidCredential {
    /// A bearer token the realm signer did not verify.
    Bearer,
    /// A header this server cannot read as a credential: unreadable bytes, an unknown scheme, or
    /// `Basic` content that does not decode to a `user:password` pair.
    Unsupported,
}

pub struct IndexReadAccess<'a> {
    index: &'a Index,
    credential: IndexCredential<'a>,
}

enum IndexCredential<'a> {
    Public,
    Acl(Principal),
    Bearer(&'a [Grant]),
    /// Whether the session's role grants carry repository read on this index. A role grant covers the
    /// whole repository, so the answer needs no per-resource match.
    Session(bool),
}

impl HeaderCredential {
    #[must_use]
    pub fn from_headers(state: &ServingState, headers: &HeaderMap) -> Self {
        let Some(header) = headers.get(header::AUTHORIZATION) else {
            return Self::Absent;
        };
        let Ok(value) = header.to_str() else {
            return Self::Invalid(InvalidCredential::Unsupported);
        };
        if let Some(token) = strip_auth_scheme(value, "Bearer") {
            return state
                .signer
                .as_ref()
                .and_then(|signer| signer.verify(token).ok())
                .map_or(Self::Invalid(InvalidCredential::Bearer), |(_, grants)| {
                    Self::Verified(VerifiedCredential::Bearer(grants))
                });
        }
        parse_basic(value).map_or(Self::Invalid(InvalidCredential::Unsupported), |credentials| {
            Self::Verified(VerifiedCredential::Basic(credentials))
        })
    }
}

/// The signed-in account for a browser session, or `None` when the request carries no usable one.
///
/// The sealed cookie holds a snapshot up to a session lifetime old, so the stored account decides:
/// a disabled or removed user has no session, and an unreadable store denies rather than trusts the
/// snapshot.
#[must_use]
pub fn session_user(state: &ServingState, headers: &HeaderMap) -> Option<ServerUser> {
    let sealer = state.session_sealer()?;
    let cookie = read_cookie(headers, SESSION_COOKIE)?;
    let snapshot = sealer.open_session(&cookie, (state.clock)())?;
    state
        .users
        .inspect(&snapshot.id)
        .ok()
        .flatten()
        .filter(|user| user.state == UserState::Active)
}

/// Returns the value of cookie `name` from the request's `Cookie` header.
#[must_use]
pub fn read_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let header = headers.get(header::COOKIE)?.to_str().ok()?;
    header.split(';').find_map(|pair| {
        let (key, value) = pair.trim().split_once('=')?;
        (key == name).then(|| value.to_owned())
    })
}

impl ReadAccess {
    /// Resolves an API credential and nothing else. Client protocol routes use this, so a browser
    /// session cannot authorize an upload or a management mutation.
    #[must_use]
    pub fn from_headers(state: &ServingState, headers: &HeaderMap) -> Self {
        Self {
            credential: header_credential(state, HeaderCredential::from_headers(state, headers)),
        }
    }

    /// The protected-read entry point shared by browser and API credentials.
    ///
    /// A present `Authorization` header decides the request on its own. Only an absent header
    /// consults the browser session, and that path re-reads the account and its role grants from
    /// metadata, so a disabled user or a revoked grant loses access on the next request rather than
    /// at cookie expiry.
    #[must_use]
    pub fn for_request(state: &ServingState, headers: &HeaderMap) -> Self {
        let header = HeaderCredential::from_headers(state, headers);
        if header == HeaderCredential::Absent
            && let Some(user) = session_user(state, headers)
            && let Ok(grants) = state.authorization.grants(&user.id)
        {
            return Self {
                credential: Credential::Session(grants),
            };
        }
        Self {
            credential: header_credential(state, header),
        }
    }

    #[must_use]
    pub fn for_index<'a>(&'a self, index: &'a Index) -> IndexReadAccess<'a> {
        let credential = if index.acl.anonymous_read {
            IndexCredential::Public
        } else {
            match &self.credential {
                Credential::Acl { credentials, now } => {
                    IndexCredential::Acl(index.acl.identify_credentials(credentials.as_ref(), *now).principal)
                }
                Credential::Bearer(grants) => IndexCredential::Bearer(grants),
                Credential::Session(grants) => IndexCredential::Session(grants_permit(
                    grants,
                    Scope::RepositoryRead,
                    &Resource::Repository(index.name.clone()),
                )),
            }
        };
        IndexReadAccess { index, credential }
    }

    #[must_use]
    pub fn search_access(&self, indexes: &[Index]) -> SearchAccess {
        let mut patterns = Vec::new();
        for index in indexes {
            let access = self.for_index(index);
            match &access.credential {
                IndexCredential::Public | IndexCredential::Session(true) => patterns.push(SearchAccessPattern {
                    route: index.route.clone(),
                    glob: "*".to_owned(),
                }),
                IndexCredential::Acl(principal) => {
                    for glob in read_globs(index, principal) {
                        patterns.push(SearchAccessPattern {
                            route: index.route.clone(),
                            glob: glob.to_owned(),
                        });
                    }
                }
                IndexCredential::Session(false) => {}
                IndexCredential::Bearer(grants) => {
                    let prefix = resource_prefix(&index.route);
                    for grant in *grants {
                        if !grant.actions.contains(&Action::Read) {
                            continue;
                        }
                        for glob in &grant.resources {
                            for remainder in glob.remainders_after(&prefix) {
                                patterns.push(SearchAccessPattern {
                                    route: index.route.clone(),
                                    glob: remainder.to_owned(),
                                });
                            }
                        }
                    }
                }
            }
        }
        SearchAccess::new(patterns)
    }
}

impl IndexReadAccess<'_> {
    /// Avoids index enumeration when the credential holds no possible read.
    ///
    /// # Errors
    /// Returns the index ACL denial when no read grant can cover a resource.
    pub fn authorize_any_resource(&self) -> Result<(), Denial> {
        match &self.credential {
            IndexCredential::Public => Ok(()),
            IndexCredential::Acl(principal) => authorize(principal, &self.index.acl, ResourceMatch::Any, Action::Read),
            IndexCredential::Session(permitted) => permitted.then_some(()).ok_or(Denial::Forbidden),
            IndexCredential::Bearer(grants) => {
                let prefix = resource_prefix(&self.index.route);
                grants
                    .iter()
                    .any(|grant| {
                        grant.actions.contains(&Action::Read)
                            && grant.resources.iter().any(|resource| resource.matches_prefix(&prefix))
                    })
                    .then_some(())
                    .ok_or(Denial::Forbidden)
            }
        }
    }

    /// # Errors
    /// Returns the index ACL denial when the credential cannot read `resource`.
    pub fn authorize_resource(&self, resource: ResourceMatch<'_>) -> Result<(), Denial> {
        match &self.credential {
            IndexCredential::Public => Ok(()),
            IndexCredential::Acl(principal) => authorize(principal, &self.index.acl, resource, Action::Read),
            IndexCredential::Session(permitted) => permitted.then_some(()).ok_or(Denial::Forbidden),
            IndexCredential::Bearer(grants) => authorize_bearer(grants, &self.index.route, resource),
        }
    }
}

fn header_credential(state: &ServingState, header: HeaderCredential) -> Credential {
    match header {
        HeaderCredential::Verified(VerifiedCredential::Bearer(grants)) => Credential::Bearer(grants),
        HeaderCredential::Verified(VerifiedCredential::Basic(credentials)) => Credential::Acl {
            credentials: Some(credentials),
            now: (state.clock)(),
        },
        HeaderCredential::Absent | HeaderCredential::Invalid(_) => Credential::Acl {
            credentials: None,
            now: (state.clock)(),
        },
    }
}

fn read_globs<'a>(index: &'a Index, principal: &'a Principal) -> Vec<&'a str> {
    let mut globs = Vec::new();
    for grant in index.acl.grants(principal) {
        if grant.actions.contains(&Action::Read) {
            for resource in &grant.resources {
                globs.push(resource.as_str());
            }
        }
    }
    globs
}

fn resource_prefix(route: &str) -> Cow<'_, str> {
    if route.is_empty() {
        Cow::Borrowed(route)
    } else {
        Cow::Owned(format!("{route}/"))
    }
}

fn resource_name<'a>(route: &str, resource: &'a str) -> Cow<'a, str> {
    if route.is_empty() {
        Cow::Borrowed(resource)
    } else {
        Cow::Owned(format!("{route}/{resource}"))
    }
}

fn authorize_bearer(grants: &[Grant], route: &str, resource: ResourceMatch<'_>) -> Result<(), Denial> {
    match resource {
        ResourceMatch::Any => {
            let prefix = resource_prefix(route);
            grants
                .iter()
                .any(|grant| {
                    grant.actions.contains(&Action::Read)
                        && grant.resources.iter().any(|resource| resource.matches_prefix(&prefix))
                })
                .then_some(())
                .ok_or(Denial::Forbidden)
        }
        ResourceMatch::Pattern(resource) => {
            let resource = resource_name(route, resource);
            authorize_grants(grants, ResourceMatch::Pattern(&resource), Action::Read)
        }
        ResourceMatch::Exact(resource) => {
            let resource = resource_name(route, resource);
            authorize_grants(grants, ResourceMatch::Exact(&resource), Action::Read)
        }
    }
}
