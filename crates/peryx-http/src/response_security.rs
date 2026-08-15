//! Keep protected fields out of serialization and caller-specific responses out of shared caches.

use std::fmt;

use axum::http::{HeaderMap, HeaderValue, header};
use peryx_driver::authz::ScopedDecision;
use serde_json::{Map, Value};

/// The least authority a response field requires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldClassification {
    Public,
    Repository,
    Operator,
    Administrator,
}

impl FieldClassification {
    const fn visible_to(self, audience: Self) -> bool {
        matches!(
            (self, audience),
            (Self::Public, _)
                | (Self::Repository, Self::Repository | Self::Administrator)
                | (Self::Operator, Self::Operator | Self::Administrator)
                | (Self::Administrator, Self::Administrator)
        )
    }
}

/// Public access, an allowed repository token, or a role decision bound to the checked scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseAuthorization {
    Public,
    /// The caller passed a repository's token ACL before response construction.
    Repository,
    Scoped(ScopedDecision),
}

impl ResponseAuthorization {
    #[must_use]
    pub const fn field_class(self) -> Option<FieldClassification> {
        match self.classification() {
            Ok(class) => Some(class),
            Err(ResponseDenied) => None,
        }
    }

    const fn classification(self) -> Result<FieldClassification, ResponseDenied> {
        let authorization = match self {
            Self::Public => return Ok(FieldClassification::Public),
            Self::Repository => return Ok(FieldClassification::Repository),
            Self::Scoped(authorization) => authorization,
        };
        if !authorization.decision().is_allowed() {
            return Err(ResponseDenied);
        }
        match authorization.scope() {
            peryx_identity::Scope::OperatorRead | peryx_identity::Scope::AnalyticsRead => {
                Ok(FieldClassification::Operator)
            }
            peryx_identity::Scope::AdministrationRead | peryx_identity::Scope::AdministrationWrite => {
                Ok(FieldClassification::Administrator)
            }
            peryx_identity::Scope::RepositoryRead
            | peryx_identity::Scope::RepositoryWrite
            | peryx_identity::Scope::RepositoryDelete => Ok(FieldClassification::Repository),
        }
    }
}

/// Couples a field with its classification so routes cannot add an unclassified value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedField {
    name: &'static str,
    classification: FieldClassification,
    value: Value,
}

impl ClassifiedField {
    #[must_use]
    pub const fn new(name: &'static str, classification: FieldClassification, value: Value) -> Self {
        Self {
            name,
            classification,
            value,
        }
    }
}

/// A generic denial that carries no resource, path, or query data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponseDenied;

impl fmt::Display for ResponseDenied {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("response access denied")
    }
}

impl std::error::Error for ResponseDenied {}

/// A route must classify every top-level field. Classify a nested object at the highest level any
/// value inside it requires, or filter that object before adding it.
///
/// # Errors
/// Returns [`ResponseDenied`] when authorization failed. The error contains no request data.
pub fn filter_fields(
    authorization: ResponseAuthorization,
    fields: impl IntoIterator<Item = ClassifiedField>,
) -> Result<Map<String, Value>, ResponseDenied> {
    let audience = authorization.classification()?;
    Ok(fields
        .into_iter()
        .filter(|field| field.classification.visible_to(audience))
        .map(|field| (field.name.to_owned(), field.value))
        .collect())
}

/// Prevent authenticated output from entering a shared cache or any cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtectedCachePolicy {
    Private,
    NoStore,
}

impl ProtectedCachePolicy {
    pub fn apply(self, headers: &mut HeaderMap) {
        headers.insert(
            header::CACHE_CONTROL,
            match self {
                Self::Private => HeaderValue::from_static("private, no-cache"),
                Self::NoStore => HeaderValue::from_static("no-store"),
            },
        );
    }
}
