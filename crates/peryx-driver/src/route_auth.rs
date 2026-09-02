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
    /// A local server user's name and password. Each operation checks that user's role against the
    /// resource it protects, so the scheme says who is asking and the route decides what they may do.
    AdministratorPassword,
}

impl ApiScheme {
    /// Every index credential the document declares, so its components section is this list rather
    /// than a copy of it.
    pub const ALL: [Self; 4] = [
        Self::IndexAccessToken,
        Self::BearerGrant,
        Self::WriteToken,
        Self::AdministratorPassword,
    ];

    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::IndexAccessToken => "indexAccessToken",
            Self::BearerGrant => "bearerGrant",
            Self::WriteToken => "writeToken",
            Self::AdministratorPassword => "administratorPassword",
        }
    }

    /// The `Authorization` scheme a client presents the credential with, and the one a `401`
    /// challenges for.
    #[must_use]
    pub const fn auth_scheme(self) -> &'static str {
        match self {
            Self::IndexAccessToken | Self::WriteToken | Self::AdministratorPassword => "Basic",
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
            Self::AdministratorPassword => HttpBuilder::new().scheme(HttpAuthScheme::Basic).description(Some(
                "A local server user's display name and password. Each operation checks the user's role \
                 against its protected resource.",
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

/// The protection space an administration route challenges in.
///
/// A realm is an RFC 7617 protection space, and clients key stored credentials on `(origin, realm)`,
/// so two surfaces sharing one realm share whatever a user entered for either. Every route here takes
/// the same credential and differs only in the role checked afterwards, which is what makes merging
/// them look like tidying; it would change what a client sends where, so they stay apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdminRealm {
    /// Server administration: repositories, caches, grants, tokens, revocations, retention and jobs.
    Server,
    /// Trashed artifacts and their records.
    Trash,
    /// Quota accounting.
    Quota,
    /// Read and byte analytics over repositories.
    Analytics,
    /// The usage summary the dashboard reads. Separate from [`Self::Analytics`] because `/+stats` is
    /// operator-scoped and answers `401` to a visitor the dashboard still has to render for.
    Stats,
    /// Recorded policy decisions.
    PolicyDecisions,
    /// The query surface.
    Query,
    /// Availability and placement control.
    Availability,
}

impl AdminRealm {
    /// Every realm, so a check can walk them rather than restate them.
    pub const ALL: [Self; 8] = [
        Self::Server,
        Self::Trash,
        Self::Quota,
        Self::Analytics,
        Self::Stats,
        Self::PolicyDecisions,
        Self::Query,
        Self::Availability,
    ];

    /// The `WWW-Authenticate` value a handler in this realm sends. The documented `401` names the
    /// same string, so the contract and the reply cannot describe different protection spaces.
    #[must_use]
    pub const fn challenge(self) -> &'static str {
        match self {
            Self::Server => "Basic realm=\"peryx-administration\"",
            Self::Trash => "Basic realm=\"peryx-trash\"",
            Self::Quota => "Basic realm=\"peryx-quota\"",
            Self::Analytics => "Basic realm=\"peryx-analytics\"",
            Self::Stats => "Basic realm=\"peryx-stats\"",
            Self::PolicyDecisions => "Basic realm=\"peryx-policy-decisions\"",
            Self::Query => "Basic realm=\"peryx-query\"",
            Self::Availability => "Basic realm=\"peryx-availability\"",
        }
    }

    /// The `401` a route in this realm answers, naming the challenge its handler sends.
    #[must_use]
    pub fn unauthorized(self) -> ResponseBuilder {
        ResponseBuilder::new().description(format!(
            "The request carried no credential this operation accepts; the reply challenges with \
             `WWW-Authenticate: {}`",
            self.challenge()
        ))
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
    /// A local server user, whose role the operation checks against the resource it protects.
    Administration,
    /// A local server user, or an index's write-granting token, so a repository's own credential
    /// reaches what an operator can see about that repository.
    WriteOrAdministration,
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
            Self::Administration => &[ApiScheme::AdministratorPassword],
            Self::WriteOrAdministration => &[ApiScheme::WriteToken, ApiScheme::AdministratorPassword],
        }
    }

    /// An operation this route serves: the credentials it accepts as alternatives, and `challenge`
    /// as the `401` it answers a request carrying none of them. A route the configuration serves to
    /// anyone answers no `401`, and drops the challenge with the requirement it does not have.
    #[must_use]
    pub fn operation(self, challenge: ResponseBuilder) -> OperationBuilder {
        self.guard(OperationBuilder::new(), Some(challenge))
    }

    /// An operation this route's credentials widen rather than gate: it serves a caller presenting
    /// nothing and narrows what it answers instead, so it declares what it accepts and challenges for
    /// nothing. `/+status` and the availability views work this way.
    #[must_use]
    pub fn widening_operation(self) -> OperationBuilder {
        self.guard(OperationBuilder::new(), None)
    }

    /// The same, applied to an operation already under construction, for the shared builders that
    /// describe a family of routes before their credentials are known.
    #[must_use]
    pub fn guard(self, operation: OperationBuilder, challenge: Option<ResponseBuilder>) -> OperationBuilder {
        let schemes = self.schemes();
        if schemes.is_empty() {
            return operation.security(SecurityRequirement::default());
        }
        let operation = schemes.iter().fold(operation, |operation, scheme| {
            operation.security(SecurityRequirement::new(scheme.name(), Vec::<String>::new()))
        });
        match challenge {
            Some(challenge) => operation.response("401", challenge),
            None => operation,
        }
    }
}
