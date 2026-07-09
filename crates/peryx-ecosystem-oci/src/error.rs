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
}

impl ErrorCode {
    /// The uppercase code string clients match on.
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
        }
    }
}

/// Build a distribution-spec error response with the code's canonical status.
#[must_use]
pub fn error_response(code: ErrorCode, message: &str) -> Response {
    let body = json!({ "errors": [{ "code": code.as_str(), "message": message }] }).to_string();
    (code.status(), [(header::CONTENT_TYPE, "application/json")], body).into_response()
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
mod tests {
    use super::{ErrorCode, error_response};
    use axum::http::StatusCode;

    #[test]
    fn test_every_code_pairs_its_wire_string_with_its_canonical_status() {
        let table = [
            (ErrorCode::BlobUnknown, "BLOB_UNKNOWN", StatusCode::NOT_FOUND),
            (
                ErrorCode::BlobUploadInvalid,
                "BLOB_UPLOAD_INVALID",
                StatusCode::BAD_REQUEST,
            ),
            (
                ErrorCode::BlobUploadUnknown,
                "BLOB_UPLOAD_UNKNOWN",
                StatusCode::NOT_FOUND,
            ),
            (ErrorCode::DigestInvalid, "DIGEST_INVALID", StatusCode::BAD_REQUEST),
            (
                ErrorCode::ManifestBlobUnknown,
                "MANIFEST_BLOB_UNKNOWN",
                StatusCode::BAD_REQUEST,
            ),
            (ErrorCode::ManifestInvalid, "MANIFEST_INVALID", StatusCode::BAD_REQUEST),
            (ErrorCode::ManifestUnknown, "MANIFEST_UNKNOWN", StatusCode::NOT_FOUND),
            (ErrorCode::NameInvalid, "NAME_INVALID", StatusCode::BAD_REQUEST),
            (ErrorCode::NameUnknown, "NAME_UNKNOWN", StatusCode::NOT_FOUND),
            (ErrorCode::SizeInvalid, "SIZE_INVALID", StatusCode::BAD_REQUEST),
            (ErrorCode::Unauthorized, "UNAUTHORIZED", StatusCode::UNAUTHORIZED),
            (ErrorCode::Denied, "DENIED", StatusCode::FORBIDDEN),
            (ErrorCode::Unsupported, "UNSUPPORTED", StatusCode::METHOD_NOT_ALLOWED),
            (
                ErrorCode::TooManyRequests,
                "TOOMANYREQUESTS",
                StatusCode::TOO_MANY_REQUESTS,
            ),
        ];
        for (code, wire, status) in table {
            assert_eq!(code.as_str(), wire);
            assert_eq!(code.status(), status);
            assert_eq!(error_response(code, "x").status(), status);
        }
    }
}
