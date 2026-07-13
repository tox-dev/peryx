//! Errors from the upstream client's fetch and range-read paths.

use url::Url;

/// An error from the range-read path.
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
    /// Whether Peryx should stop trying ranges for this index and fall back to full downloads.
    #[must_use]
    pub const fn disables_ranges(&self) -> bool {
        matches!(self, Self::Unsupported | Self::Invalid(_))
    }
}

/// An error talking to an upstream index.
#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    #[error(transparent)]
    Url(#[from] url::ParseError),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error("missing upstream Simple API Content-Type from {url}")]
    MissingContentType { url: Url },
    #[error("unsupported upstream Simple API Content-Type {content_type:?} from {url}")]
    UnsupportedContentType { url: Url, content_type: String },
    #[error("upstream response exceeds the {limit}-byte limit")]
    ResponseTooLarge { limit: usize },
}

impl UpstreamError {
    /// The HTTP status attached to a transport error, when reqwest has one.
    #[must_use]
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Http(err) => err.status().map(|status| status.as_u16()),
            Self::Url(_)
            | Self::MissingContentType { .. }
            | Self::UnsupportedContentType { .. }
            | Self::ResponseTooLarge { .. } => None,
        }
    }
}

impl UpstreamError {
    /// Error text safe for user-visible responses: status and failure class, without URLs that may
    /// contain credentials or signed query strings.
    #[must_use]
    pub fn user_message(&self) -> String {
        match self {
            Self::Url(err) => format!("invalid upstream URL: {err}"),
            Self::Http(err) if let Some(status) = err.status() => format!("upstream returned {status}"),
            Self::Http(err) if err.is_timeout() => "upstream request timed out".to_owned(),
            Self::Http(err) if err.is_connect() => "upstream connection failed".to_owned(),
            Self::Http(err) if err.is_decode() => "upstream response could not be decoded".to_owned(),
            Self::Http(_) => "upstream request failed".to_owned(),
            Self::MissingContentType { .. } => "upstream response missed Simple API Content-Type".to_owned(),
            Self::UnsupportedContentType { .. } => "upstream returned unsupported Simple API Content-Type".to_owned(),
            Self::ResponseTooLarge { limit } => format!("upstream response exceeds the {limit}-byte limit"),
        }
    }
}
