//! Neutral HTTP surfaces share this resolver so presentation routes enforce index ACLs.

use std::borrow::Cow;

use axum::http::{HeaderMap, header};
use peryx_identity::{Action, Denial, Grant, Principal, ResourceMatch, authorize, authorize_grants, strip_auth_scheme};
use peryx_search::{SearchAccess, SearchAccessPattern};

use crate::{Index, ServingState};

pub struct ReadAccess {
    credential: Credential,
}

enum Credential {
    Acl { header: Option<String>, now: i64 },
    Bearer(Vec<Grant>),
}

pub struct IndexReadAccess<'a> {
    index: &'a Index,
    credential: IndexCredential<'a>,
}

enum IndexCredential<'a> {
    Public,
    Acl(Principal),
    Bearer(&'a [Grant]),
}

impl ReadAccess {
    #[must_use]
    pub fn from_headers(state: &ServingState, headers: &HeaderMap) -> Self {
        let header = headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok());
        let credential = if let Some(token) = header.and_then(|value| strip_auth_scheme(value, "Bearer"))
            && let Some(signer) = &state.signer
            && let Ok((_, grants)) = signer.verify(token)
        {
            Credential::Bearer(grants)
        } else {
            Credential::Acl {
                header: header.map(str::to_owned),
                now: (state.clock)(),
            }
        };
        Self { credential }
    }

    #[must_use]
    pub fn for_index<'a>(&'a self, index: &'a Index) -> IndexReadAccess<'a> {
        let credential = if index.acl.anonymous_read {
            IndexCredential::Public
        } else {
            match &self.credential {
                Credential::Acl { header, now } => {
                    IndexCredential::Acl(index.acl.identify(header.as_deref(), *now).principal)
                }
                Credential::Bearer(grants) => IndexCredential::Bearer(grants),
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
                IndexCredential::Public => patterns.push(SearchAccessPattern {
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
            IndexCredential::Bearer(grants) => authorize_bearer(grants, &self.index.route, resource),
        }
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
