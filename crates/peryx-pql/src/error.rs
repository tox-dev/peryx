#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusClass {
    BadRequest,
    NotFound,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PqlError {
    #[error("could not parse the query")]
    Parse(String),
    #[error("the query is not valid")]
    Validation(String),
    #[error("a query parameter was not supplied")]
    MissingParameter(String),
    #[error("the query is too expensive to run")]
    CostExceeded(String),
    #[error("the join cannot be bounded and is refused")]
    UnboundedJoin(String),
    #[error("the pagination cursor is not valid")]
    InvalidCursor,
    #[error("the caller's scope changed; restart the query")]
    CursorScopeChanged,
    #[error("not found")]
    Unauthorized,
    #[error("the query backend is unavailable")]
    Backend(String),
}

impl PqlError {
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
            Self::Backend(_) => StatusClass::Unavailable,
        }
    }
}
