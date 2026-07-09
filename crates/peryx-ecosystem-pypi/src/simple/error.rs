//! Errors raised while parsing an upstream Simple API document.

use std::fmt;

/// An upstream Simple API document could not be used.
#[derive(Debug)]
pub enum SimpleError {
    /// The document was not valid JSON for the Simple API model.
    Json(serde_json::Error),
    /// The document was too large for the HTML parser.
    Html(tl::ParseError),
    /// The upstream advertised a backwards-incompatible Simple API major version.
    UnsupportedApiVersion(String),
    /// The upstream advertised a malformed Simple API version.
    InvalidApiVersion(String),
    /// The upstream advertised an unknown project status marker.
    InvalidProjectStatus(String),
}

impl fmt::Display for SimpleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(err) => err.fmt(f),
            Self::Html(err) => write!(f, "invalid upstream Simple API HTML: {err}"),
            Self::UnsupportedApiVersion(version) => write!(
                f,
                "unsupported upstream Simple API version {version:?}; peryx supports Simple API 1.x"
            ),
            Self::InvalidApiVersion(version) => {
                write!(
                    f,
                    "invalid upstream Simple API version {version:?}; expected Major.Minor"
                )
            }
            Self::InvalidProjectStatus(status) => {
                write!(f, "invalid upstream project status marker {status:?}")
            }
        }
    }
}

impl std::error::Error for SimpleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(err) => Some(err),
            Self::Html(err) => Some(err),
            Self::UnsupportedApiVersion(_) | Self::InvalidApiVersion(_) | Self::InvalidProjectStatus(_) => None,
        }
    }
}

impl From<serde_json::Error> for SimpleError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}

impl From<tl::ParseError> for SimpleError {
    fn from(err: tl::ParseError) -> Self {
        Self::Html(err)
    }
}
