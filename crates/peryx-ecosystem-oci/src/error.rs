//! The distribution-spec error response: `{"errors":[{"code","message","detail"}]}`, with each code
//! bound to its canonical HTTP status so a handler cannot pair the wrong status with a code.

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// A distribution-spec error code (the uppercase wire value).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    BlobUnknown,
    BlobUploadInvalid,
    BlobUploadUnknown,
    DigestInvalid,
    ManifestBlobUnknown,
    ManifestInvalid,
    ManifestUnknown,
    NameInvalid,
    NameUnknown,
    SizeInvalid,
    Unauthorized,
    Denied,
    Unsupported,
    TooManyRequests,
    Unavailable,
}

impl ErrorCode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BlobUnknown => "BLOB_UNKNOWN",
            Self::BlobUploadInvalid => "BLOB_UPLOAD_INVALID",
            Self::BlobUploadUnknown => "BLOB_UPLOAD_UNKNOWN",
            Self::DigestInvalid => "DIGEST_INVALID",
            Self::ManifestBlobUnknown => "MANIFEST_BLOB_UNKNOWN",
            Self::ManifestInvalid => "MANIFEST_INVALID",
            Self::ManifestUnknown => "MANIFEST_UNKNOWN",
            Self::NameInvalid => "NAME_INVALID",
            Self::NameUnknown => "NAME_UNKNOWN",
            Self::SizeInvalid => "SIZE_INVALID",
            Self::Unauthorized => "UNAUTHORIZED",
            Self::Denied => "DENIED",
            Self::Unsupported => "UNSUPPORTED",
            Self::TooManyRequests => "TOOMANYREQUESTS",
            Self::Unavailable => "UNAVAILABLE",
        }
    }

    /// The canonical HTTP status the spec pairs with this code.
    #[must_use]
    pub const fn status(self) -> StatusCode {
        match self {
            Self::BlobUnknown | Self::BlobUploadUnknown | Self::ManifestUnknown | Self::NameUnknown => {
                StatusCode::NOT_FOUND
            }
            Self::BlobUploadInvalid
            | Self::DigestInvalid
            | Self::ManifestBlobUnknown
            | Self::ManifestInvalid
            | Self::NameInvalid
            | Self::SizeInvalid => StatusCode::BAD_REQUEST,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Denied => StatusCode::FORBIDDEN,
            Self::Unsupported => StatusCode::METHOD_NOT_ALLOWED,
            Self::TooManyRequests => StatusCode::TOO_MANY_REQUESTS,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        }
    }
}

/// Build a distribution-spec error response with the code's canonical status.
#[must_use]
pub fn error_response(code: ErrorCode, message: &str) -> Response {
    error_response_with_status(code.status(), code, message)
}

/// Build a distribution-spec error body under a status the code does not canonically pair with. The
/// spec has no "payload too large" code, so an oversize manifest borrows `SIZE_INVALID` yet answers
/// `413` instead of that code's usual `400`.
#[must_use]
pub fn error_response_with_status(status: StatusCode, code: ErrorCode, message: &str) -> Response {
    let body = json!({ "errors": [{ "code": code.as_str(), "message": message }] }).to_string();
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}

/// A `502` for an upstream that failed or answered unexpectedly, so a pull-through miss reports a
/// gateway fault rather than masquerading as a client error the puller would not retry.
#[must_use]
pub fn gateway_error(message: &str) -> Response {
    let body = json!({ "errors": [{ "code": "UNKNOWN", "message": message }] }).to_string();
    (
        StatusCode::BAD_GATEWAY,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

#[cfg(test)]
#[path = "../tests/unit/error/tests.rs"]
mod tests;
