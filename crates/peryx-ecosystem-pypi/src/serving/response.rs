//! Response mapping: negotiated list/detail/file responses and cache-error status and body.
#![allow(
    clippy::result_large_err,
    reason = "handler helpers carry an axum Response as their error; boxing it everywhere adds noise"
)]

use std::fmt::Write as _;

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use peryx_policy::PolicyDenial;

use crate::cache::CacheError;
use crate::{ProjectDetail, ProjectList, render_index_html, to_json};

use super::{Format, MIME_HTML, MIME_JSON, MIME_LEGACY_JSON};

/// Map a project-list result to a negotiated response. Sync so every arm is directly testable.
pub fn index_response(result: Result<ProjectList, CacheError>, format: Format, index: &str) -> Response {
    let list = match result {
        Ok(list) => list,
        Err(err) => return cache_error_response(&err, CacheContext::list(index)),
    };
    let vary = (header::VARY, "Accept");
    match format {
        Format::Json => ([(header::CONTENT_TYPE, MIME_JSON), vary], to_json(&list)).into_response(),
        Format::Html => ([(header::CONTENT_TYPE, MIME_HTML), vary], render_index_html(&list)).into_response(),
    }
}

/// Serve an already-rendered PEP 503 page, from the hot cache or from the render that just filled it.
pub(super) fn html_bytes_response(body: bytes::Bytes) -> Response {
    ([(header::CONTENT_TYPE, MIME_HTML), (header::VARY, "Accept")], body).into_response()
}

/// Serve an already-rendered legacy JSON document.
pub(super) fn legacy_bytes_response(body: bytes::Bytes) -> Response {
    ([(header::CONTENT_TYPE, MIME_LEGACY_JSON)], body).into_response()
}

/// The PEP 691 JSON representation of a project, or the status its resolution earned.
///
/// Only JSON: the HTML render is served (and cached) by the route, because rendering it twice to
/// discover which of the two representations was asked for is the cost this cache exists to remove.
pub fn detail_response(result: Result<Option<ProjectDetail>, CacheError>, index: &str, project: &str) -> Response {
    let detail = match result {
        Ok(Some(detail)) => detail,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                format!("project {project:?} was not found on index {index:?}"),
            )
                .into_response();
        }
        Err(CacheError::Upstream(
            err @ (peryx_upstream::UpstreamError::MissingContentType { .. }
            | peryx_upstream::UpstreamError::UnsupportedContentType { .. }),
        )) => {
            tracing::warn!(error = ?err, "unsupported upstream simple api content type");
            return (StatusCode::BAD_GATEWAY, err.to_string()).into_response();
        }
        Err(err) => {
            tracing::error!(error = ?err, "project detail failed");
            return cache_error_response(&err, CacheContext::project(index, project));
        }
    };
    (
        [(header::CONTENT_TYPE, MIME_JSON), (header::VARY, "Accept")],
        to_json(&detail),
    )
        .into_response()
}

pub(super) fn legacy_json_response(
    result: Result<Option<ProjectDetail>, CacheError>,
    index: &str,
    project: &str,
    version: Option<&str>,
) -> Response {
    match result {
        // The route renders and answers a project that resolves, so reaching here with one means the
        // release it asked for is not among that project's files.
        Ok(Some(_)) => (
            StatusCode::NOT_FOUND,
            format!("version {version:?} was not found for project {project:?} on index {index:?}"),
        )
            .into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            format!("project {project:?} was not found on index {index:?}"),
        )
            .into_response(),
        Err(CacheError::Upstream(
            err @ (peryx_upstream::UpstreamError::MissingContentType { .. }
            | peryx_upstream::UpstreamError::UnsupportedContentType { .. }),
        )) => {
            tracing::warn!(error = ?err, "unsupported upstream simple api content type");
            (StatusCode::BAD_GATEWAY, err.to_string()).into_response()
        }
        Err(err) => {
            tracing::error!(error = ?err, "legacy project json failed");
            cache_error_response(&err, CacheContext::project(index, project))
        }
    }
}

/// Map a file-bytes result to a response. Sync so every arm is directly unit-testable.
pub fn file_response(result: Result<bytes::Bytes, CacheError>, context: CacheContext<'_>) -> Response {
    match result {
        Ok(body) => (
            [
                (header::CONTENT_TYPE, "application/octet-stream"),
                (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
            ],
            body,
        )
            .into_response(),
        Err(err) => cache_error_response(&err, context),
    }
}

#[derive(Clone, Copy)]
pub struct CacheContext<'a> {
    operation: &'static str,
    index: Option<&'a str>,
    project: Option<&'a str>,
    digest: Option<&'a str>,
    filename: Option<&'a str>,
}

impl<'a> CacheContext<'a> {
    pub(super) const fn list(index: &'a str) -> Self {
        Self {
            operation: "project list",
            index: Some(index),
            project: None,
            digest: None,
            filename: None,
        }
    }

    pub(super) const fn project(index: &'a str, project: &'a str) -> Self {
        Self {
            operation: "project detail",
            index: Some(index),
            project: Some(project),
            digest: None,
            filename: None,
        }
    }

    pub(super) const fn file(index: &'a str, digest: &'a str, filename: &'a str) -> Self {
        Self {
            operation: "file download",
            index: Some(index),
            project: None,
            digest: Some(digest),
            filename: Some(filename),
        }
    }

    pub(super) const fn metadata(index: &'a str, digest: &'a str, filename: &'a str) -> Self {
        Self {
            operation: "metadata fetch",
            index: Some(index),
            project: None,
            digest: Some(digest),
            filename: Some(filename),
        }
    }

    pub(super) const fn upload(index: &'a str, project: &'a str) -> Self {
        Self {
            operation: "upload storage",
            index: Some(index),
            project: Some(project),
            digest: None,
            filename: None,
        }
    }

    pub(super) const fn mutation(operation: &'static str) -> Self {
        Self {
            operation,
            index: None,
            project: None,
            digest: None,
            filename: None,
        }
    }
}

pub(super) fn cache_error_response(err: &CacheError, context: CacheContext<'_>) -> Response {
    if let CacheError::Policy(denial) = err {
        return policy_denial_response(denial);
    }
    if let CacheError::RateLimited { retry_after } = err {
        let mut response = (cache_error_status(err, &context), cache_error_message(err, context)).into_response();
        response.headers_mut().insert(
            header::RETRY_AFTER,
            HeaderValue::from_str(&retry_after.to_string()).expect("integer retry-after is a valid header"),
        );
        return response;
    }
    (cache_error_status(err, &context), cache_error_message(err, context)).into_response()
}

pub(super) fn policy_denial_response(denial: &PolicyDenial) -> Response {
    (
        StatusCode::FORBIDDEN,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_vec(denial).expect("policy denial always serializes"),
    )
        .into_response()
}

fn cache_error_status(err: &CacheError, context: &CacheContext<'_>) -> StatusCode {
    match err {
        CacheError::Meta(_) | CacheError::Blob(_) | CacheError::MissingSha256(_) => StatusCode::INTERNAL_SERVER_ERROR,
        CacheError::FileNotFound | CacheError::NoPromotableFiles { .. } => StatusCode::NOT_FOUND,
        CacheError::OfflineMissing(_) => StatusCode::SERVICE_UNAVAILABLE,
        CacheError::FileExists(_) => StatusCode::CONFLICT,
        CacheError::NotVolatile | CacheError::Policy(_) => StatusCode::FORBIDDEN,
        CacheError::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
        CacheError::Parse(_) if matches!(context.operation, "upload storage" | "file removal" | "promotion") => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
        CacheError::Upstream(_)
        | CacheError::Archive(_)
        | CacheError::Parse(_)
        | CacheError::Simple(_)
        | CacheError::Unavailable
        | CacheError::Stream(_) => StatusCode::BAD_GATEWAY,
    }
}

fn cache_error_message(err: &CacheError, context: CacheContext<'_>) -> String {
    let mut message = context.operation.to_owned();
    if let Some(index) = context.index {
        let _ = write!(message, " on index {index:?}");
    }
    if let Some(project) = context.project {
        let _ = write!(message, " for project {project:?}");
    }
    if let Some(filename) = context.filename {
        let _ = write!(message, " for file {filename:?}");
    }
    if let Some(digest) = context.digest {
        let _ = write!(message, " with digest {digest}");
    }
    message.push_str(": ");
    message.push_str(&err.user_message());
    message
}

#[cfg(test)]
mod tests {
    use peryx_storage::blob::BlobError;
    use peryx_storage::meta::MetaError;

    use super::*;

    #[test]
    fn test_cache_error_status_maps_store_and_policy_errors() {
        let context = CacheContext::mutation("file removal");
        assert_eq!(
            cache_error_status(&CacheError::Meta(meta_error()), &context),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            cache_error_status(
                &CacheError::Blob(BlobError::NotFound("sha256:abc".to_owned())),
                &context
            ),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(
            cache_error_status(&CacheError::FileExists("pkg-1.0.whl".to_owned()), &context),
            StatusCode::CONFLICT
        );
        assert_eq!(
            cache_error_status(&CacheError::NotVolatile, &context),
            StatusCode::FORBIDDEN
        );
    }

    fn meta_error() -> MetaError {
        MetaError::Decode(serde_json::from_str::<serde_json::Value>("not json").unwrap_err())
    }
}
