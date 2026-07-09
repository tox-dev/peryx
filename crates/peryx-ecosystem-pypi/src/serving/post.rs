//! The multipart upload handler: authorization, policy and status checks, and storage.
#![allow(
    clippy::result_large_err,
    reason = "handler helpers carry an axum Response as their error; boxing it everywhere adds noise"
)]

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::Multipart;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use peryx_http::handlers::not_found;
use peryx_http::metrics::Event;
use peryx_http::state::{AppState, Index};
use peryx_http::webhook::{WebhookEvent, WebhookEventKind};
use peryx_policy::PolicyAction;

use crate::cache::{self, CacheError};
use crate::policy::PypiPolicy;
use crate::upload::{self, UploadError};
use crate::{ProjectStatus, normalize_name};

use super::response::{CacheContext, cache_error_response, policy_denial_response};
use super::upload_form::{collect_form, upload_error_message, upload_error_response};
use super::{authorize, request_id, upload_target};

/// `POST /{route}/`, the legacy multipart upload API, used unchanged by twine and `uv publish`.
pub async fn pypi_dispatch_post(
    state: Arc<AppState>,
    path: String,
    headers: HeaderMap,
    multipart: Multipart,
) -> Response {
    state.requests.fetch_add(1, Ordering::Relaxed);
    let actor = peryx_http::security::actor(&headers);
    let Some((index, rest)) = state.resolve(&path) else {
        return not_found();
    };
    if !rest.is_empty() {
        security_upload_event(&headers, actor.as_deref(), &index.route, None, "denied")
            .reason(Some("upload path must target an index root"))
            .emit();
        return not_found();
    }
    let Some(hosted) = upload_target(&state, index) else {
        security_upload_event(&headers, actor.as_deref(), &index.route, None, "denied")
            .reason(Some("index does not accept uploads"))
            .emit();
        return (StatusCode::METHOD_NOT_ALLOWED, "index does not accept uploads").into_response();
    };
    if let Err(response) = authorize(hosted, &headers) {
        return response;
    }
    accept_upload(&state, index, hosted, &headers, actor.as_deref(), multipart).await
}

async fn accept_upload(
    state: &Arc<AppState>,
    index: &Index,
    hosted: &Index,
    headers: &HeaderMap,
    actor: Option<&str>,
    multipart: Multipart,
) -> Response {
    let (form, staged) = match collect_form(multipart, &state.blobs).await {
        Ok(form) => form,
        Err(response) => {
            security_upload_event(headers, actor, &index.route, Some(&hosted.name), "failure")
                .reason(Some("multipart body rejected"))
                .emit();
            return response;
        }
    };
    let Some(staged) = staged else {
        let err = UploadError::Missing("content");
        let (_, reason) = upload_error_message(&err);
        security_upload_event(headers, actor, &index.route, Some(&hosted.name), "denied")
            .project(form.name.as_deref().map(normalize_name).as_deref())
            .version(form.version.as_deref())
            .reason(Some(&reason))
            .emit();
        return upload_error_response(&err);
    };
    let form_project = form.name.as_deref().map(normalize_name);
    let form_version = form.version.clone();
    let form_filename = form.filename.clone();
    let upload_time_unix = (state.clock)();
    let prepared = match upload::prepare(form, staged, &index.route, upload_time_unix) {
        Ok(prepared) => prepared,
        Err(err) => {
            let (_, reason) = upload_error_message(&err);
            security_upload_event(headers, actor, &index.route, Some(&hosted.name), "denied")
                .project(form_project.as_deref())
                .version(form_version.as_deref())
                .filename(form_filename.as_deref())
                .reason(Some(&reason))
                .emit();
            return upload_error_response(&err);
        }
    };
    let project = prepared.normalized.clone();
    let version = prepared.record.version.clone();
    let filename = prepared.filename.clone();
    let digest = prepared.digest.as_str().to_owned();
    let audit = UploadAudit {
        headers,
        actor: actor.map(str::to_owned),
        request_id: request_id(headers),
        created_at_unix: upload_time_unix,
        index: &index.name,
        route: &index.route,
        hosted: &hosted.name,
        project: &project,
        version: &version,
        filename: &filename,
        digest: &digest,
    };
    if let Some(block) = upload_policy_response(index, &prepared, &audit) {
        return block;
    }
    if hosted.name != index.name
        && let Some(block) = upload_policy_response(hosted, &prepared, &audit)
    {
        return block;
    }
    if let Some(block) = upload_status_response(
        cache::project_status(state, index, &project).await,
        &index.route,
        &project,
    ) {
        emit_upload_status_event(&audit, &block);
        return block.response;
    }
    upload_store_response(state, &audit, cache::store_upload(state, &hosted.name, prepared))
}

fn upload_policy_response(
    index: &Index,
    prepared: &upload::PreparedUpload,
    audit: &UploadAudit<'_>,
) -> Option<Response> {
    index
        .policy
        .check_file(PolicyAction::Upload, &prepared.normalized, &prepared.record.file)
        .err()
        .map(|denial| {
            security_upload_event(
                audit.headers,
                audit.actor.as_deref(),
                audit.route,
                Some(audit.hosted),
                "denied",
            )
            .project(Some(audit.project))
            .version(Some(audit.version))
            .filename(Some(audit.filename))
            .digest(Some(audit.digest))
            .reason(Some(&denial.reason))
            .emit();
            policy_denial_response(&denial)
        })
}

struct UploadAudit<'a> {
    headers: &'a HeaderMap,
    actor: Option<String>,
    request_id: Option<String>,
    created_at_unix: i64,
    index: &'a str,
    route: &'a str,
    hosted: &'a str,
    project: &'a str,
    version: &'a str,
    filename: &'a str,
    digest: &'a str,
}

fn upload_store_response(state: &Arc<AppState>, audit: &UploadAudit<'_>, result: Result<bool, CacheError>) -> Response {
    match result {
        Ok(stored) => {
            if stored {
                state.metrics.record(Event::Upload {
                    route: audit.route.to_owned(),
                    project: audit.project.to_owned(),
                });
                peryx_http::webhook::emit(
                    state.clone(),
                    &WebhookEvent {
                        kind: WebhookEventKind::Upload,
                        created_at_unix: audit.created_at_unix,
                        index: audit.index.to_owned(),
                        route: audit.route.to_owned(),
                        hosted_index: audit.hosted.to_owned(),
                        project: audit.project.to_owned(),
                        version: Some(audit.version.to_owned()),
                        filename: Some(audit.filename.to_owned()),
                        digest: Some(audit.digest.to_owned()),
                        count: 1,
                        actor: audit.actor.clone(),
                        request_id: audit.request_id.clone(),
                    },
                );
            }
            security_upload_event(
                audit.headers,
                audit.actor.as_deref(),
                audit.route,
                Some(audit.hosted),
                if stored { "success" } else { "noop" },
            )
            .project(Some(audit.project))
            .version(Some(audit.version))
            .filename(Some(audit.filename))
            .digest(Some(audit.digest))
            .count(usize::from(stored))
            .reason((!stored).then_some("same content already stored"))
            .emit();
            (StatusCode::OK, "upload accepted").into_response()
        }
        Err(CacheError::FileExists(filename)) => {
            security_upload_event(
                audit.headers,
                audit.actor.as_deref(),
                audit.route,
                Some(audit.hosted),
                "denied",
            )
            .project(Some(audit.project))
            .version(Some(audit.version))
            .filename(Some(&filename))
            .digest(Some(audit.digest))
            .reason(Some("file exists with different content"))
            .emit();
            (
                StatusCode::BAD_REQUEST,
                format!("File already exists: {filename:?} has different content; use a different filename"),
            )
                .into_response()
        }
        Err(err) => {
            let reason = err.user_message();
            security_upload_event(
                audit.headers,
                audit.actor.as_deref(),
                audit.route,
                Some(audit.hosted),
                "failure",
            )
            .project(Some(audit.project))
            .version(Some(audit.version))
            .filename(Some(audit.filename))
            .digest(Some(audit.digest))
            .reason(Some(&reason))
            .emit();
            tracing::error!(error = ?err, "upload store failed");
            cache_error_response(&err, CacheContext::upload(audit.route, audit.project))
        }
    }
}

fn emit_upload_status_event(audit: &UploadAudit<'_>, block: &UploadStatusBlock) {
    security_upload_event(
        audit.headers,
        audit.actor.as_deref(),
        audit.route,
        Some(audit.hosted),
        block.result,
    )
    .project(Some(audit.project))
    .version(Some(audit.version))
    .filename(Some(audit.filename))
    .digest(Some(audit.digest))
    .reason(Some(&block.reason))
    .emit();
}

pub(super) struct UploadStatusBlock {
    pub(super) response: Response,
    pub(super) result: &'static str,
    pub(super) reason: String,
}

pub(super) fn upload_status_response(
    result: Result<ProjectStatus, CacheError>,
    index: &str,
    project: &str,
) -> Option<UploadStatusBlock> {
    match result {
        Ok(status) if status.allows_uploads() => None,
        Ok(status) => {
            let reason = format!("project {project:?} is {}; uploads are disabled", status.marker());
            Some(UploadStatusBlock {
                response: (StatusCode::FORBIDDEN, reason.clone()).into_response(),
                result: "denied",
                reason,
            })
        }
        Err(err) => {
            let reason = err.user_message();
            Some(UploadStatusBlock {
                response: cache_error_response(&err, CacheContext::upload(index, project)),
                result: "failure",
                reason,
            })
        }
    }
}

fn security_upload_event<'a>(
    headers: &'a HeaderMap,
    actor: Option<&'a str>,
    route: &'a str,
    hosted_index: Option<&'a str>,
    result: &'static str,
) -> peryx_http::security::Event<'a> {
    let event = peryx_http::security::Event::new("upload", result)
        .actor(actor)
        .index(route)
        .request(headers);
    if let Some(hosted_index) = hosted_index {
        event.hosted_index(hosted_index)
    } else {
        event
    }
}

#[cfg(test)]
mod tests {
    use peryx_storage::meta::MetaError;

    use super::*;

    #[test]
    fn test_upload_status_response_maps_policy_and_store_errors() {
        assert!(upload_status_response(Ok(ProjectStatus::Active), "root/pypi", "flask").is_none());
        let archived = upload_status_response(Ok(ProjectStatus::Archived), "root/pypi", "flask").unwrap();
        assert_eq!(archived.response.status(), StatusCode::FORBIDDEN);
        assert_eq!(archived.result, "denied");
        assert_eq!(archived.reason, "project \"flask\" is archived; uploads are disabled");

        let failure = upload_status_response(Err(CacheError::Meta(meta_error())), "root/pypi", "flask").unwrap();
        assert_eq!(failure.response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(failure.result, "failure");
        assert!(failure.reason.contains("metadata store error"));
    }

    fn meta_error() -> MetaError {
        MetaError::Decode(serde_json::from_str::<serde_json::Value>("not json").unwrap_err())
    }
}
