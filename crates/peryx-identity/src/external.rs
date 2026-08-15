use std::collections::HashSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{GrantScope, Role, ServerUser, UserName};

const MAX_PROVIDER_ID_BYTES: usize = 128;
const MAX_SUBJECT_BYTES: usize = 1_024;
const MAX_GROUP_BYTES: usize = 256;
pub const MAX_EXTERNAL_GROUPS: usize = 256;

/// Provider IDs retain operator-defined spelling.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProviderId(String);

impl ProviderId {
    /// # Errors
    /// Returns an error when the identifier is empty, too long, or contains characters outside the
    /// ASCII letters, digits, `.`, `_`, and `-`.
    pub fn new(value: &str) -> Result<Self, ExternalIdentityError> {
        if value.is_empty() {
            return Err(ExternalIdentityError::EmptyProviderId);
        }
        if value.len() > MAX_PROVIDER_ID_BYTES {
            return Err(ExternalIdentityError::ProviderIdTooLong);
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        {
            return Err(ExternalIdentityError::InvalidProviderId);
        }
        Ok(Self(value.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ProviderId {
    type Error = ExternalIdentityError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value)
    }
}

impl From<ProviderId> for String {
    fn from(value: ProviderId) -> Self {
        value.0
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Provider subjects remain opaque and case-sensitive.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ExternalSubject(String);

impl ExternalSubject {
    /// # Errors
    /// Returns an error when the subject is empty, too long, or contains a control character.
    pub fn new(value: &str) -> Result<Self, ExternalIdentityError> {
        validate_opaque(
            value,
            MAX_SUBJECT_BYTES,
            ExternalIdentityError::EmptySubject,
            ExternalIdentityError::SubjectTooLong,
        )?;
        if value.chars().any(char::is_control) {
            return Err(ExternalIdentityError::InvalidSubject);
        }
        Ok(Self(value.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ExternalSubject {
    type Error = ExternalIdentityError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value)
    }
}

impl From<ExternalSubject> for String {
    fn from(value: ExternalSubject) -> Self {
        value.0
    }
}

impl fmt::Debug for ExternalSubject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ExternalSubject([redacted])")
    }
}

/// External groups use exact matching without case folding or Unicode normalization.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ExternalGroup(String);

impl ExternalGroup {
    /// # Errors
    /// Returns an error when the name is empty, too long, or contains a control character.
    pub fn new(value: &str) -> Result<Self, ExternalIdentityError> {
        validate_opaque(
            value,
            MAX_GROUP_BYTES,
            ExternalIdentityError::EmptyGroup,
            ExternalIdentityError::GroupTooLong,
        )?;
        if value.chars().any(char::is_control) {
            return Err(ExternalIdentityError::InvalidGroup);
        }
        Ok(Self(value.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ExternalGroup {
    type Error = ExternalIdentityError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value)
    }
}

impl From<ExternalGroup> for String {
    fn from(value: ExternalGroup) -> Self {
        value.0
    }
}

impl fmt::Debug for ExternalGroup {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ExternalGroup([redacted])")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExternalIdentity {
    pub provider: ProviderId,
    pub subject: ExternalSubject,
}

impl ExternalIdentity {
    #[must_use]
    pub const fn new(provider: ProviderId, subject: ExternalSubject) -> Self {
        Self { provider, subject }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalGroupGrant {
    pub group: ExternalGroup,
    pub role: Role,
    pub scope: GrantScope,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedRoleGrant {
    pub role: Role,
    pub scope: GrantScope,
}

/// Callers must verify the provider transport before constructing this assertion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalLogin {
    pub identity: ExternalIdentity,
    pub display_name: UserName,
    pub groups: Vec<ExternalGroup>,
}

impl ExternalLogin {
    /// # Errors
    /// Returns an error when the assertion contains too many groups.
    pub fn new(
        identity: ExternalIdentity,
        display_name: UserName,
        groups: Vec<ExternalGroup>,
    ) -> Result<Self, ExternalIdentityError> {
        if groups.len() > MAX_EXTERNAL_GROUPS {
            return Err(ExternalIdentityError::TooManyGroups);
        }
        Ok(Self {
            identity,
            display_name,
            groups,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalLinkRequest {
    pub identity: ExternalIdentity,
    pub display_name: UserName,
    pub grants: Vec<ManagedRoleGrant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalIdentityResolution {
    pub user: ServerUser,
    pub link_created: bool,
    pub grants_changed: bool,
}

pub trait ExternalIdentityStore {
    type Error;

    /// # Errors
    /// Returns the implementation's error when the atomic operation fails.
    fn link_or_resolve(&self, request: ExternalLinkRequest) -> Result<ExternalIdentityResolution, Self::Error>;
}

impl<E, F> ExternalIdentityStore for F
where
    F: Fn(ExternalLinkRequest) -> Result<ExternalIdentityResolution, E>,
{
    type Error = E;

    fn link_or_resolve(&self, request: ExternalLinkRequest) -> Result<ExternalIdentityResolution, Self::Error> {
        self(request)
    }
}

#[derive(Debug, Clone)]
pub struct ExternalIdentityLinker<S> {
    store: S,
}

impl<S: ExternalIdentityStore> ExternalIdentityLinker<S> {
    #[must_use]
    pub const fn new(store: S) -> Self {
        Self { store }
    }

    /// # Errors
    /// Returns the storage implementation's error when the atomic link operation fails.
    pub fn link_or_resolve(
        &self,
        login: &ExternalLogin,
        mappings: &[ExternalGroupGrant],
    ) -> Result<ExternalIdentityResolution, S::Error> {
        let groups = login.groups.iter().collect::<HashSet<_>>();
        let mut grants = mappings
            .iter()
            .filter(|mapping| groups.contains(&mapping.group))
            .map(|mapping| ManagedRoleGrant {
                role: mapping.role,
                scope: mapping.scope.clone(),
            })
            .collect::<Vec<_>>();
        grants.sort_by(|left, right| grant_key(left).cmp(&grant_key(right)));
        grants.dedup();
        self.store.link_or_resolve(ExternalLinkRequest {
            identity: login.identity.clone(),
            display_name: login.display_name.clone(),
            grants,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ExternalIdentityError {
    #[error("external provider ID cannot be empty")]
    EmptyProviderId,
    #[error("external provider ID exceeds 128 bytes")]
    ProviderIdTooLong,
    #[error("external provider ID contains an unsupported character")]
    InvalidProviderId,
    #[error("external subject cannot be empty")]
    EmptySubject,
    #[error("external subject exceeds 1024 bytes")]
    SubjectTooLong,
    #[error("external subject contains a control character")]
    InvalidSubject,
    #[error("external group cannot be empty")]
    EmptyGroup,
    #[error("external group exceeds 256 bytes")]
    GroupTooLong,
    #[error("external group contains a control character")]
    InvalidGroup,
    #[error("external login exceeds 256 groups")]
    TooManyGroups,
}

const fn validate_opaque(
    value: &str,
    max_bytes: usize,
    empty: ExternalIdentityError,
    too_long: ExternalIdentityError,
) -> Result<(), ExternalIdentityError> {
    if value.is_empty() {
        return Err(empty);
    }
    if value.len() > max_bytes {
        return Err(too_long);
    }
    Ok(())
}

fn grant_key(grant: &ManagedRoleGrant) -> (&str, &str, &str) {
    match &grant.scope {
        GrantScope::Server => (grant.role.as_str(), "server", ""),
        GrantScope::Repository { name } => (grant.role.as_str(), "repository", name),
    }
}
