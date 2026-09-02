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
    #[error("upstream bounded read deadline exceeded")]
    DeadlineExceeded,
    /// A streaming transfer fell below the rate its own delivered bytes had earned it.
    ///
    /// Distinct from [`Self::DeadlineExceeded`], which bounds a composed metadata read, and from a
    /// transport timeout, which a caller may resume at the offset it reached. This ends the transfer.
    #[error("upstream delivered {delivered} bytes below the sustained throughput floor")]
    BelowThroughputFloor { delivered: u64 },
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
            | Self::DeadlineExceeded
            | Self::BelowThroughputFloor { .. }
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
            Self::DeadlineExceeded => "upstream request timed out".to_owned(),
            Self::BelowThroughputFloor { .. } => "upstream transfer was too slow to finish".to_owned(),
            Self::BlockedDestination { .. } => "upstream destination is not permitted".to_owned(),
        }
    }
}
