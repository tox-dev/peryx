use std::fmt::Write as _;

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use peryx_policy::PolicyDenial;

use crate::cache::{CacheError, DetailPage, ProvenanceBody};
use crate::store::AttestationAvailability;
use crate::{ProjectList, render_index_html, to_json};

use super::{Format, MIME_HTML, MIME_JSON, MIME_LEGACY_JSON};

#[derive(Clone, Copy)]
pub(super) struct PageSerial(pub(super) Option<u64>);

pub(super) fn page_serial(response: &Response) -> Option<PageSerial> {
    response.extensions().get::<PageSerial>().copied()
}

/// Negotiated project-list response.
pub fn index_response(result: Result<(ProjectList, Option<u64>), CacheError>, format: Format, index: &str) -> Response {
    let (list, last_serial) = match result {
        Ok(page) => page,
        Err(err) => return cache_error_response(&err, CacheContext::list(index)),
    };
    let vary = (header::VARY, "Accept");
    let response = match format {
        Format::Json => ([(header::CONTENT_TYPE, MIME_JSON), vary], to_json(&list)).into_response(),
        Format::Html => ([(header::CONTENT_TYPE, MIME_HTML), vary], render_index_html(&list)).into_response(),
    };
    with_last_serial(response, last_serial)
}

pub(super) fn json_bytes_response(body: impl IntoResponse, last_serial: Option<u64>) -> Response {
    with_last_serial(
        ([(header::CONTENT_TYPE, MIME_JSON), (header::VARY, "Accept")], body).into_response(),
        last_serial,
    )
}

/// Serve an already-rendered PEP 503 page, from the hot cache or from the render that just filled it.
pub(super) fn html_bytes_response(body: bytes::Bytes, last_serial: Option<u64>) -> Response {
    with_last_serial(
        ([(header::CONTENT_TYPE, MIME_HTML), (header::VARY, "Accept")], body).into_response(),
        last_serial,
    )
}

/// Serve an already-rendered legacy JSON document.
pub(super) fn legacy_bytes_response(body: bytes::Bytes, last_serial: Option<u64>) -> Response {
    with_last_serial(
        ([(header::CONTENT_TYPE, MIME_LEGACY_JSON)], body).into_response(),
        last_serial,
    )
}

/// The PEP 691 JSON representation of a project, or the status its resolution earned.
///
/// Only JSON: the HTML render is served (and cached) by the route, because rendering it twice to
/// discover which of the two representations was asked for is the cost this cache exists to remove.
pub fn detail_response(result: Result<Option<DetailPage>, CacheError>, index: &str, project: &str) -> Response {
    let page = match result {
        Ok(Some(page)) => page,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                format!("project {project:?} was not found on index {index:?}"),
            )
                .into_response();
        }
        Err(CacheError::Upstream(err @ peryx_upstream::UpstreamError::InvalidResponse { .. })) => {
            tracing::warn!(error = ?err, "unsupported upstream simple api content type");
            return (StatusCode::BAD_GATEWAY, err.user_message()).into_response();
        }
        Err(err) => {
            tracing::error!(error = ?err, "project detail failed");
            return cache_error_response(&err, CacheContext::project(index, project));
        }
    };
    with_last_serial(
        (
            [(header::CONTENT_TYPE, MIME_JSON), (header::VARY, "Accept")],
            to_json(&page.detail),
        )
            .into_response(),
        page.last_serial,
    )
}

pub(super) fn legacy_json_response(
    result: Result<Option<DetailPage>, CacheError>,
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
        Err(CacheError::Upstream(err @ peryx_upstream::UpstreamError::InvalidResponse { .. })) => {
            tracing::warn!(error = ?err, "unsupported upstream simple api content type");
            (StatusCode::BAD_GATEWAY, err.user_message()).into_response()
        }
        Err(err) => {
            tracing::error!(error = ?err, "legacy project json failed");
            cache_error_response(&err, CacheContext::project(index, project))
        }
    }
}

fn with_last_serial(mut response: Response, last_serial: Option<u64>) -> Response {
    response.extensions_mut().insert(PageSerial(last_serial));
    if let Some(last_serial) = last_serial {
        response.headers_mut().insert(
            axum::http::HeaderName::from_static("x-pypi-last-serial"),
            HeaderValue::from_str(&last_serial.to_string()).expect("a u64 is a valid header value"),
        );
    }
    response
}

/// Artifact response.
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

/// Map a provenance result to a response without describing an upstream document as verified.
pub fn provenance_response(result: Result<ProvenanceBody, CacheError>, context: CacheContext<'_>) -> Response {
    match result {
        Ok(body) => {
            let mut response = body.bytes.into_response();
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_str(&body.media_type).expect("an accepted media type is a valid header"),
            );
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static(if body.immutable {
                    "public, max-age=31536000, immutable"
                } else {
                    "no-cache"
                }),
            );
            response.headers_mut().insert(
                axum::http::HeaderName::from_static("x-peryx-provenance-source"),
                HeaderValue::from_str(&body.source).unwrap_or(HeaderValue::from_static("upstream")),
            );
            response.headers_mut().insert(
                axum::http::HeaderName::from_static("x-peryx-provenance-availability"),
                HeaderValue::from_static(match body.availability {
                    AttestationAvailability::Cached => "cached",
                    AttestationAvailability::RemoteOnly => "remote-only",
                }),
            );
            response
        }
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

    pub(super) const fn provenance(index: &'a str, digest: &'a str, filename: &'a str) -> Self {
        Self {
            operation: "provenance fetch",
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
    if let CacheError::UpstreamRateLimited { retry_after } = err {
        let mut response = (cache_error_status(err, &context), cache_error_message(err, context)).into_response();
        if let Some(retry_after) = retry_after {
            response.headers_mut().insert(
                header::RETRY_AFTER,
                HeaderValue::from_str(retry_after).expect("the upstream retry-after header was valid"),
            );
        }
        return response;
    }
    (cache_error_status(err, &context), cache_error_message(err, context)).into_response()
}

pub(super) fn policy_denial_response(denial: &PolicyDenial) -> Response {
    (
        StatusCode::FORBIDDEN,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_vec(&PypiPolicyDenial::from(denial)).expect("policy denial always serializes"),
    )
        .into_response()
}

#[derive(serde::Serialize)]
struct PypiPolicyDenial<'a> {
    action: &'a peryx_policy::PolicyAction,
    project: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    filename: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<&'a str>,
    rule: &'a str,
    field: &'a str,
    reason: String,
}

impl<'a> From<&'a PolicyDenial> for PypiPolicyDenial<'a> {
    fn from(denial: &'a PolicyDenial) -> Self {
        Self {
            action: &denial.action,
            project: &denial.resource,
            filename: denial.artifact.as_deref(),
            version: denial.group.as_deref(),
            rule: pypi_rule(denial.rule),
            field: pypi_field(denial.field),
            reason: pypi_reason(&denial.reason),
        }
    }
}

fn pypi_rule(rule: &str) -> &str {
    match rule {
        "resource-allow-list" => "project-allow-list",
        "resource-block-list" => "project-block-list",
        "max-artifact-size" => "max-file-size",
        _ => rule,
    }
}

fn pypi_field(field: &str) -> &str {
    match field {
        "resource" => "project",
        "artifact" => "filename",
        "group" => "version",
        _ => field,
    }
}

pub fn pypi_reason(reason: &str) -> String {
    reason.replace("resource ", "project ").replace("artifact ", "file ")
}

fn cache_error_status(err: &CacheError, context: &CacheContext<'_>) -> StatusCode {
    match err {
        CacheError::Meta(_)
        | CacheError::Blob(_)
        | CacheError::MissingSha256(_)
        | CacheError::Quota(_)
        | CacheError::VirtualIndexCycle(_) => StatusCode::INTERNAL_SERVER_ERROR,
        CacheError::FileNotFound | CacheError::ArtifactRevoked | CacheError::NoPromotableFiles { .. } => {
            StatusCode::NOT_FOUND
        }
        CacheError::OfflineMissing(_) => StatusCode::SERVICE_UNAVAILABLE,
        CacheError::FileExists(_) | CacheError::AuthoritySuperseded => StatusCode::CONFLICT,
        CacheError::NotVolatile | CacheError::Policy(_) => StatusCode::FORBIDDEN,
        CacheError::RateLimited { .. } | CacheError::UpstreamRateLimited { .. } => StatusCode::TOO_MANY_REQUESTS,
        CacheError::Parse(_) if matches!(context.operation, "upload storage" | "file removal" | "promotion") => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
        CacheError::Upstream(_)
        | CacheError::Archive(_)
        | CacheError::Parse(_)
        | CacheError::Simple(_)
        | CacheError::Unavailable
        | CacheError::InvalidProvenance
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
#[path = "../../tests/unit/serving/response/tests.rs"]
mod tests;
