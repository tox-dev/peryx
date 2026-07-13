//! Neutral HTTP surfaces share this resolver so presentation routes enforce index ACLs.

use std::borrow::Cow;

use axum::http::{HeaderMap, header};
use peryx_identity::{Action, Denial, Grant, Principal, authorize, authorize_grants, strip_auth_scheme};
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
                    patterns.extend(read_globs(index, principal).map(|glob| SearchAccessPattern {
                        route: index.route.clone(),
                        glob: glob.to_owned(),
                    }));
                }
                IndexCredential::Bearer(grants) => {
                    let prefix = resource_prefix(&index.route);
                    for glob in grants
                        .iter()
                        .filter(|grant| grant.actions.contains(&Action::Read))
                        .flat_map(|grant| &grant.projects)
                    {
                        patterns.extend(glob.remainders_after(&prefix).map(|glob| SearchAccessPattern {
                            route: index.route.clone(),
                            glob: glob.to_owned(),
                        }));
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
    /// Returns the index ACL denial when no read grant can cover a project.
    pub fn authorize_any_project(&self) -> Result<(), Denial> {
        match &self.credential {
            IndexCredential::Public => Ok(()),
            IndexCredential::Acl(principal) => authorize(principal, &self.index.acl, None, Action::Read),
            IndexCredential::Bearer(grants) => {
                let prefix = resource_prefix(&self.index.route);
                grants
                    .iter()
                    .any(|grant| {
                        grant.actions.contains(&Action::Read)
                            && grant.projects.iter().any(|project| project.matches_prefix(&prefix))
                    })
                    .then_some(())
                    .ok_or(Denial::Forbidden)
            }
        }
    }

    /// Prevents a repository grant from exposing its siblings.
    ///
    /// # Errors
    /// Returns the index ACL denial when the credential cannot read `project`.
    pub fn authorize_project(&self, project: &str) -> Result<(), Denial> {
        match &self.credential {
            IndexCredential::Public => Ok(()),
            IndexCredential::Acl(principal) => authorize(principal, &self.index.acl, Some(project), Action::Read),
            IndexCredential::Bearer(grants) => {
                authorize_grants(grants, Some(&resource_name(&self.index.route, project)), Action::Read)
            }
        }
    }
}

fn read_globs<'a>(index: &'a Index, principal: &'a Principal) -> impl Iterator<Item = &'a str> {
    index
        .acl
        .grants(principal)
        .iter()
        .filter(|grant| grant.actions.contains(&Action::Read))
        .flat_map(|grant| grant.projects.iter().map(peryx_identity::Glob::as_str))
}

fn resource_prefix(route: &str) -> Cow<'_, str> {
    if route.is_empty() {
        Cow::Borrowed(route)
    } else {
        Cow::Owned(format!("{route}/"))
    }
}

fn resource_name<'a>(route: &str, project: &'a str) -> Cow<'a, str> {
    if route.is_empty() {
        Cow::Borrowed(project)
    } else {
        Cow::Owned(format!("{route}/{project}"))
    }
}
