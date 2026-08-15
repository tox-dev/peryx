use super::CredentialError;

#[derive(Debug, thiserror::Error)]
pub enum RangeError {
    #[error(transparent)]
    Upstream(#[from] UpstreamError),
    #[error("upstream does not support byte range requests")]
    Unsupported,
    #[error("upstream returned an invalid byte range response: {0}")]
    Invalid(String),
}

impl RangeError {
    #[must_use]
    pub const fn disables_ranges(&self) -> bool {
        matches!(self, Self::Unsupported | Self::Invalid(_))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    #[error("upstream credential refresh failed: {0}")]
    Credential(#[from] CredentialError),
    #[error(transparent)]
    Url(#[from] url::ParseError),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error("invalid upstream response: {reason}")]
    InvalidResponse { reason: String },
    #[error("upstream response exceeds the {limit}-byte limit")]
    ResponseTooLarge { limit: usize },
    #[error("upstream destination is not permitted: {reason}")]
    BlockedDestination { reason: String },
}

impl UpstreamError {
    #[must_use]
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Http(err) => err.status().map(|status| status.as_u16()),
            Self::Credential(_)
            | Self::Url(_)
            | Self::InvalidResponse { .. }
            | Self::ResponseTooLarge { .. }
            | Self::BlockedDestination { .. } => None,
        }
    }
}

impl UpstreamError {
    /// Returns user-safe text without URLs, credentials, or signed query strings.
    #[must_use]
    pub fn user_message(&self) -> String {
        match self {
            Self::Credential(_) => "upstream credential refresh failed".to_owned(),
            Self::Url(err) => format!("invalid upstream URL: {err}"),
            Self::Http(err) if let Some(status) = err.status() => format!("upstream returned {status}"),
            Self::Http(err) if err.is_timeout() => "upstream request timed out".to_owned(),
            Self::Http(err) if err.is_connect() => "upstream connection failed".to_owned(),
            Self::Http(err) if err.is_decode() => "upstream response could not be decoded".to_owned(),
            Self::Http(_) => "upstream request failed".to_owned(),
            Self::InvalidResponse { .. } => "upstream returned an invalid response".to_owned(),
            Self::ResponseTooLarge { limit } => format!("upstream response exceeds the {limit}-byte limit"),
            Self::BlockedDestination { .. } => "upstream destination is not permitted".to_owned(),
        }
    }
}
