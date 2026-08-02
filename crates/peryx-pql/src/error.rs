//! The closed error set and its mapping to a transport-neutral status class.
//!
//! Errors carry a category and a short, safe clause description — never a parameter value and never
//! the raw query text — so a denial or a diagnostic can be surfaced without echoing a secret the
//! caller embedded in a predicate.

/// A transport-neutral status class the HTTP layer maps to a concrete code.
///
/// Keeping the mapping here, next to the errors, means the wire layer never re-derives which failure
/// is a client mistake versus a server condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusClass {
    /// A malformed or invalid query, an over-budget plan, or a stale cursor: the caller must change
    /// the request.
    BadRequest,
    /// The domain is not visible under the caller's scope; existence is not disclosed.
    NotFound,
    /// The backing store could not answer.
    Unavailable,
    /// The feature parsed and validated but is not wired in this build.
    NotImplemented,
}

/// Every way a query can fail. The set is closed so the wire layer's match is exhaustive.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PqlError {
    /// The text did not parse. The message names the syntactic problem, not the offending value.
    #[error("could not parse the query")]
    Parse(String),
    /// A parsed query referenced something the catalog does not allow: an unknown domain, an unknown
    /// column, a type mismatch, or a misuse of an aggregate.
    #[error("the query is not valid")]
    Validation(String),
    /// A named parameter was used in the query but not supplied.
    #[error("a query parameter was not supplied")]
    MissingParameter(String),
    /// The plan's estimated cost is over budget; the message names the expensive clause.
    #[error("the query is too expensive to run")]
    CostExceeded(String),
    /// A join could not be bounded by an index on its key, so it is refused rather than run as a
    /// scan.
    #[error("the join cannot be bounded and is refused")]
    UnboundedJoin(String),
    /// The join grammar parsed but join execution is not wired in this build.
    #[error("joins are not available yet")]
    JoinUnavailable,
    /// The opaque cursor is malformed.
    #[error("the pagination cursor is not valid")]
    InvalidCursor,
    /// The cursor was minted under a different scope; the caller must restart the query.
    #[error("the caller's scope changed; restart the query")]
    CursorScopeChanged,
    /// The caller is not authorized for the domain; existence is not disclosed.
    #[error("not found")]
    Unauthorized,
    /// The backing store failed.
    #[error("the query backend is unavailable")]
    Backend(String),
}

impl PqlError {
    /// The status class the wire layer answers with.
    #[must_use]
    pub const fn status(&self) -> StatusClass {
        match self {
            Self::Parse(_)
            | Self::Validation(_)
            | Self::MissingParameter(_)
            | Self::CostExceeded(_)
            | Self::UnboundedJoin(_)
            | Self::InvalidCursor
            | Self::CursorScopeChanged => StatusClass::BadRequest,
            Self::Unauthorized => StatusClass::NotFound,
            Self::JoinUnavailable => StatusClass::NotImplemented,
            Self::Backend(_) => StatusClass::Unavailable,
        }
    }
}
