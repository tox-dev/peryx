//! One description of the credentials a route takes, read by the handler that enforces them and by
//! the `OpenAPI` document that declares them.
//!
//! A contract generated from a second, hand-kept list drifts from the routes it describes. Both sides
//! read the values here instead, so a route that changes what it accepts changes its declaration with
//! it.

use utoipa::openapi::path::OperationBuilder;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::openapi::{ResponseBuilder, SecurityRequirement};

use crate::Index;

/// The `Basic` challenge an index route answers. Handlers send it, and
/// [`ApiScheme::IndexAccessToken`] and [`ApiScheme::WriteToken`] declare what satisfies it.
pub const BASIC_CHALLENGE: &str = "Basic realm=\"peryx\"";

/// A credential the contract declares, and the `Authorization` scheme it arrives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiScheme {
    /// An access token of the target hosted index. A read is satisfied by read authority alone, which
    /// is why no read names the write-granting scheme.
    IndexAccessToken,
    /// A grant token this deployment minted: a scoped API token, or one an OCI client obtained from
    /// the token realm.
    BearerGrant,
    /// A write-granting access token of the target hosted index.
    WriteToken,
}

impl ApiScheme {
    /// Every index credential the document declares, so its components section is this list rather
    /// than a copy of it.
    pub const ALL: [Self; 3] = [Self::IndexAccessToken, Self::BearerGrant, Self::WriteToken];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::IndexAccessToken => "indexAccessToken",
            Self::BearerGrant => "bearerGrant",
            Self::WriteToken => "writeToken",
        }
    }

    /// The `Authorization` scheme a client presents the credential with, and the one a `401`
    /// challenges for.
    #[must_use]
    pub const fn auth_scheme(self) -> &'static str {
        match self {
            Self::IndexAccessToken | Self::WriteToken => "Basic",
            Self::BearerGrant => "Bearer",
        }
    }

    #[must_use]
    pub fn declaration(self) -> SecurityScheme {
        let http = match self {
            Self::IndexAccessToken => HttpBuilder::new().scheme(HttpAuthScheme::Basic).description(Some(
                "The password is an access token of the target hosted index, granting reads of the \
                 requested resource.",
            )),
            Self::BearerGrant => HttpBuilder::new()
                .scheme(HttpAuthScheme::Bearer)
                .bearer_format("JWT")
                .description(Some(
                    "A grant token this deployment minted: a scoped API token, or one an OCI client \
                     obtained from `GET /v2/token`.",
                )),
            Self::WriteToken => HttpBuilder::new().scheme(HttpAuthScheme::Basic).description(Some(
                "The password is a write-granting access token of the hosted index.",
            )),
        };
        SecurityScheme::Http(http.build())
    }
}

/// Whether the selected configuration serves reads to callers presenting nothing.
///
/// Index ACLs answer that one index at a time while a single document describes the whole deployment,
/// so one index that refuses anonymous reads makes every read operation carry its credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadExposure {
    Public,
    Protected,
}

impl ReadExposure {
    #[must_use]
    pub fn of(indexes: &[Index]) -> Self {
        if indexes.iter().all(|index| index.acl.anonymous_read) {
            Self::Public
        } else {
            Self::Protected
        }
    }
}

/// What a route accepts, and what it answers to a request carrying nothing it accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteAuth {
    /// An index ACL read, as the selected configuration exposes it.
    Read(ReadExposure),
    /// A write on the target hosted index. A write-granting access token authorizes it, as does a
    /// bearer grant carrying the action.
    Write,
}

impl RouteAuth {
    /// The credentials the route accepts, as alternatives. Empty where the configuration serves the
    /// read to anyone, which the document spells as the empty security requirement.
    #[must_use]
    pub const fn schemes(self) -> &'static [ApiScheme] {
        match self {
            Self::Read(ReadExposure::Public) => &[],
            Self::Read(ReadExposure::Protected) => &[ApiScheme::IndexAccessToken, ApiScheme::BearerGrant],
            Self::Write => &[ApiScheme::WriteToken, ApiScheme::BearerGrant],
        }
    }

    /// An operation this route serves: the credentials it accepts as alternatives, and `challenge`
    /// as the `401` it answers a request carrying none of them. A route the configuration serves to
    /// anyone answers no `401`, and drops the challenge with the requirement it does not have.
    #[must_use]
    pub fn operation(self, challenge: ResponseBuilder) -> OperationBuilder {
        let schemes = self.schemes();
        if schemes.is_empty() {
            return OperationBuilder::new().security(SecurityRequirement::default());
        }
        schemes
            .iter()
            .fold(OperationBuilder::new(), |operation, scheme| {
                operation.security(SecurityRequirement::new(scheme.name(), Vec::<String>::new()))
            })
            .response("401", challenge)
    }
}
