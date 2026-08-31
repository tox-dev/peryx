use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures_util::FutureExt;
use peryx_core::path::{self};
use peryx_driver::access::ReadAccess;
use peryx_driver::not_found;
use peryx_driver::state::ServingState;
use peryx_identity::{Action, ResourceMatch};
use peryx_index::{Index, IndexKind};

use crate::cache::{self, CacheError};
use crate::webhook::{self, PypiWebhook};
use crate::{Yanked, normalize_name};

use super::HttpResult;
use super::post::{UploadStatusBlock, upload_status_response};
use super::response::{CacheContext, cache_error_response};
use super::{authorize, identify, path_error_response, request_id, upload_target};

#[derive(Clone, Copy)]
struct PromotionAudit<'a> {
    headers: &'a HeaderMap,
    actor: Option<&'a str>,
    route: &'a str,
    source_index: &'a str,
    hosted_index: &'a str,
    project: &'a str,
    version: &'a str,
}

fn emit_promotion_status_event(audit: &PromotionAudit<'_>, block: &UploadStatusBlock) {
    emit_promotion_refusal(audit, block.result, &block.reason);
}

fn emit_promotion_refusal(audit: &PromotionAudit<'_>, result: &'static str, reason: &str) {
    peryx_events::security::Event::new("promote", result)
        .actor(audit.actor)
        .index(audit.route)
        .source_index(audit.source_index)
        .hosted_index(audit.hosted_index)
        .resource(Some(audit.project))
        .group(Some(audit.version))
        .reason(Some(reason))
        .request(audit.headers)
        .emit();
}

fn promotion_source_route(query: Option<&str>) -> HttpResult<String> {
    let Some(query) = query else {
        return Err((StatusCode::BAD_REQUEST, "promotion requires from={source route}")
            .into_response()
            .into());
    };
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if key == "from" && !value.is_empty() {
            return Ok(value.into_owned());
        }
    }
    Err((StatusCode::BAD_REQUEST, "promotion requires from={source route}")
        .into_response()
        .into())
}

fn promotion_source<'a>(state: &'a ServingState, route: &str) -> HttpResult<&'a Index> {
    let route = route.trim_matches('/');
    state
        .indexes
        .iter()
        .find(|index| index.route == route)
        .ok_or_else(not_found)
        .map_err(Into::into)
}

/// `PUT /{route}/{project}/[{version}/]yank` marks files yanked (PEP 592, reversible);
/// `PUT .../restore` clears the hidden marker a DELETE left on read-only upstream files.
pub async fn pypi_dispatch_put(state: Arc<ServingState>, uri: axum::http::Uri, headers: HeaderMap) -> Response {
    state.requests.fetch_add(1, Ordering::Relaxed);
    let path = uri.path().trim_start_matches('/');
    let (index, hosted, spec, identity) = match removal_target(&state, path, &headers, Action::Write) {
        Ok(target) => target,
        Err(error) => return error.into_response(),
    };
    let actor = peryx_events::security::actor(&identity);
    if let Some(spec) = strip_action_segment(spec, "promote") {
        return promote_request(&state, index, hosted, spec, uri.query(), &headers, actor.as_deref())
            .boxed()
            .await;
    }
    if let Some(spec) = strip_action_segment(spec, "yank") {
        return yank_request(&state, index, hosted, spec, uri.query(), &headers, actor.as_deref())
            .boxed()
            .await;
    }
    if let Some(spec) = strip_action_segment(spec, "restore") {
        return restore_request(&state, index, hosted, spec, &headers, actor.as_deref())
            .boxed()
            .await;
    }
    not_found()
}

async fn promote_request(
    state: &Arc<ServingState>,
    index: &Index,
    hosted: &Index,
    spec: &str,
    query: Option<&str>,
    headers: &HeaderMap,
    actor: Option<&str>,
) -> Response {
    let source_route = match promotion_source_route(query) {
        Ok(route) => route,
        Err(error) => return error.into_response(),
    };
    let source = match promotion_source(state, &source_route) {
        Ok(source) => source,
        Err(error) => return error.into_response(),
    };
    let (project, version) = match parse_project_version(spec) {
        Ok((project, Some(version))) => (project, version),
        Ok((_project, None)) => return (StatusCode::BAD_REQUEST, "promotion requires a version").into_response(),
        Err(error) => return error.into_response(),
    };
    let audit = PromotionAudit {
        headers,
        actor,
        route: &index.route,
        source_index: &source.route,
        hosted_index: &hosted.name,
        project: &project,
        version: &version,
    };
    // Without this, write on the target alone copies a private release into an index the caller can
    // download from. The named route's own ACL decides, not the hosted layer a virtual source writes
    // through, and a refusal answers like a missing source so the two stay indistinguishable.
    if ReadAccess::from_headers(state, headers)
        .for_index(source)
        .authorize_resource(ResourceMatch::Pattern(&project))
        .is_err()
    {
        emit_promotion_refusal(&audit, "denied", "source read denied");
        return not_found();
    }
    let Some(source_local) = upload_target(state, source) else {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            format!("source index {source_route:?} has no hosted upload layer"),
        )
            .into_response();
    };
    if let Some(block) = upload_status_response(
        cache::project_status(state, index, &project).await,
        &index.route,
        &project,
    ) {
        emit_promotion_status_event(&audit, &block);
        return block.response;
    }
    let result = cache::promote_release(
        state,
        &source_local.name,
        &hosted.name,
        &index.route,
        &project,
        &version,
    )
    .await;
    security_promotion_event(audit, &result);
    promotion_response(result)
}

async fn yank_request(
    state: &Arc<ServingState>,
    index: &Index,
    hosted: &Index,
    spec: &str,
    query: Option<&str>,
    headers: &HeaderMap,
    actor: Option<&str>,
) -> Response {
    let (project, version) = match parse_project_version(spec) {
        Ok(parsed) => parsed,
        Err(error) => return error.into_response(),
    };
    let audit = MutationAudit {
        headers,
        action: "yank",
        actor,
        index: &index.name,
        route: &index.route,
        hosted_index: &hosted.name,
        project: &project,
        version: version.as_deref(),
        request_id: request_id(headers),
    };
    let result = cache::set_yanked_with_webhook(
        state,
        index,
        &hosted.name,
        &project,
        version.as_deref(),
        yank_marker(query),
        |count| prepare_mutation_webhook(state, webhook::YANK, &audit, count),
    )
    .await;
    security_mutation_event(&audit, &result);
    notify_mutation_webhook(state, &result);
    count_response(result)
}

async fn restore_request(
    state: &Arc<ServingState>,
    index: &Index,
    hosted: &Index,
    spec: &str,
    headers: &HeaderMap,
    actor: Option<&str>,
) -> Response {
    let (project, version) = match parse_project_version(spec) {
        Ok(parsed) => parsed,
        Err(error) => return error.into_response(),
    };
    let audit = MutationAudit {
        headers,
        action: "restore",
        actor,
        index: &index.name,
        route: &index.route,
        hosted_index: &hosted.name,
        project: &project,
        version: version.as_deref(),
        request_id: request_id(headers),
    };
    let result = cache::restore_files_with_webhook(state, &hosted.name, &project, version.as_deref(), |count| {
        prepare_mutation_webhook(state, webhook::RESTORE, &audit, count)
    })
    .await;
    security_mutation_event(&audit, &result);
    notify_mutation_webhook(state, &result);
    count_response(result)
}

/// `DELETE /{route}/{project}/[{version}/]` removes files: uploads are soft-deleted to trash (volatile
/// indexes only), read-only upstream files are hidden reversibly. A `.../yank` suffix un-yanks.
pub async fn pypi_dispatch_delete(state: Arc<ServingState>, uri: axum::http::Uri, headers: HeaderMap) -> Response {
    state.requests.fetch_add(1, Ordering::Relaxed);
    let path = uri.path().trim_start_matches('/');
    let (index, hosted, spec, identity) = match removal_target(&state, path, &headers, Action::Delete) {
        Ok(target) => target,
        Err(error) => return error.into_response(),
    };
    let actor = peryx_events::security::actor(&identity);
    if let Some(spec) = strip_action_segment(spec, "yank") {
        let (project, version) = match parse_project_version(spec) {
            Ok(parsed) => parsed,
            Err(error) => return error.into_response(),
        };
        let audit = MutationAudit {
            headers: &headers,
            action: "unyank",
            actor: actor.as_deref(),
            index: &index.name,
            route: &index.route,
            hosted_index: &hosted.name,
            project: &project,
            version: version.as_deref(),
            request_id: request_id(&headers),
        };
        let result = cache::set_yanked_with_webhook(
            &state,
            index,
            &hosted.name,
            &project,
            version.as_deref(),
            Yanked::No,
            |count| prepare_mutation_webhook(&state, webhook::UNYANK, &audit, count),
        )
        .await;
        security_mutation_event(&audit, &result);
        notify_mutation_webhook(&state, &result);
        return count_response(result);
    }
    let (project, version) = match parse_project_version(spec) {
        Ok(parsed) => parsed,
        Err(error) => return error.into_response(),
    };
    let volatile = is_volatile(hosted);
    let reason = delete_reason(uri.query());
    let trash = cache::TrashContext {
        deleted_at_unix: (state.clock)(),
        actor: actor.as_deref(),
        reason: reason.as_deref(),
    };
    let audit = MutationAudit {
        headers: &headers,
        action: "delete",
        actor: actor.as_deref(),
        index: &index.name,
        route: &index.route,
        hosted_index: &hosted.name,
        project: &project,
        version: version.as_deref(),
        request_id: request_id(&headers),
    };
    let result = cache::remove_files_with_webhook(
        &state,
        index,
        &hosted.name,
        &project,
        version.as_deref(),
        cache::RemovalContext { volatile, trash },
        |count| prepare_mutation_webhook(&state, webhook::DELETE, &audit, count),
    )
    .await;
    security_mutation_event(&audit, &result);
    notify_mutation_webhook(&state, &result);
    count_response(result)
}

fn removal_target<'a>(
    state: &'a ServingState,
    path: &'a str,
    headers: &HeaderMap,
    action: Action,
) -> HttpResult<(&'a Index, &'a Index, &'a str, super::UploadIdentity)> {
    let (index, rest) = state.resolve(path).ok_or_else(not_found)?;
    let hosted = upload_target(state, index)
        .ok_or_else(|| (StatusCode::METHOD_NOT_ALLOWED, "index is read-only").into_response())?;
    let identity = identify(state, index, headers);
    authorize(
        &index.route,
        hosted,
        &identity,
        mutation_project(rest).as_deref(),
        action,
        headers,
    )?;
    Ok((index, hosted, rest, identity))
}

fn mutation_project(spec: &str) -> Option<String> {
    let project = spec.split('/').find(|segment| !segment.is_empty())?;
    Some(normalize_name(project))
}

const fn is_volatile(hosted: &Index) -> bool {
    matches!(hosted.kind, IndexKind::Hosted { volatile: true, .. })
}

struct MutationAudit<'a> {
    headers: &'a HeaderMap,
    action: &'static str,
    actor: Option<&'a str>,
    index: &'a str,
    route: &'a str,
    hosted_index: &'a str,
    project: &'a str,
    version: Option<&'a str>,
    request_id: Option<String>,
}

fn security_mutation_event(audit: &MutationAudit<'_>, result: &Result<usize, CacheError>) {
    let (security_result, count, reason) = match result {
        Ok(0) => ("noop", 0, Some("nothing matched".to_owned())),
        Ok(count) => ("success", *count, None),
        Err(CacheError::NotVolatile) => ("denied", 0, Some(CacheError::NotVolatile.user_message())),
        Err(err) => ("failure", 0, Some(err.user_message())),
    };
    peryx_events::security::Event::new(audit.action, security_result)
        .actor(audit.actor)
        .index(audit.route)
        .hosted_index(audit.hosted_index)
        .resource(Some(audit.project))
        .group(audit.version)
        .count(count)
        .reason(reason.as_deref())
        .request(audit.headers)
        .emit();
}

fn security_promotion_event(audit: PromotionAudit<'_>, result: &Result<usize, CacheError>) {
    let (security_result, count, reason) = match result {
        Ok(0) => ("noop", 0, Some("same files already exist on target".to_owned())),
        Ok(count) => ("success", *count, None),
        Err(err @ (CacheError::FileExists(_) | CacheError::NoPromotableFiles { .. })) => {
            ("denied", 0, Some(err.user_message()))
        }
        Err(err) => ("failure", 0, Some(err.user_message())),
    };
    peryx_events::security::Event::new("promote", security_result)
        .actor(audit.actor)
        .index(audit.route)
        .source_index(audit.source_index)
        .hosted_index(audit.hosted_index)
        .resource(Some(audit.project))
        .group(Some(audit.version))
        .count(count)
        .reason(reason.as_deref())
        .request(audit.headers)
        .emit();
}

fn prepare_mutation_webhook(
    state: &ServingState,
    event: &'static str,
    audit: &MutationAudit<'_>,
    count: usize,
) -> Option<peryx_storage::meta::WebhookEventIntent> {
    let created_at_unix = (state.clock)();
    webhook::prepare(
        state,
        PypiWebhook {
            event,
            created_at_unix,
            index: audit.index,
            route: audit.route,
            hosted_index: audit.hosted_index,
            project: audit.project,
            version: audit.version,
            filename: None,
            digest: None,
            count,
            actor: audit.actor,
            request_id: audit.request_id.as_deref(),
        },
    )
}

fn notify_mutation_webhook(state: &ServingState, result: &Result<usize, CacheError>) {
    if result.as_ref().is_ok_and(|count| *count > 0) {
        peryx_events::webhook::notify(state);
    }
}

/// Peel a trailing `/{action}` off the spec, but only when a project segment precedes it. A project
/// whose PEP 503 name is itself `yank`/`restore`/`promote` must stay addressable at the project
/// level, so the action grammar never claims the whole spec.
fn strip_action_segment<'a>(spec: &'a str, action: &str) -> Option<&'a str> {
    let spec = spec.trim_end_matches('/');
    let base = spec.strip_suffix(action)?;
    base.ends_with('/').then_some(base)
}

fn parse_project_version(spec: &str) -> HttpResult<(String, Option<String>)> {
    let trimmed = spec.trim_matches('/');
    let mut parts = trimmed.splitn(2, '/');
    let project = parts
        .next()
        .map(path::decode_path_segment)
        .transpose()
        .map_err(|err| path_error_response(&err))?
        .unwrap_or_default()
        .into_owned();
    path::validate_path_segment("project", &project).map_err(|err| path_error_response(&err))?;
    let version = parts
        .next()
        .map(|version| path::decode_path(version.trim_matches('/')))
        .transpose()
        .map_err(|err| path_error_response(&err))?
        .filter(|version| !version.is_empty())
        .map(std::borrow::Cow::into_owned);
    if let Some(version) = &version {
        path::validate_path_segment("version", version).map_err(|err| path_error_response(&err))?;
    }
    Ok((normalize_name(&project), version))
}

fn delete_reason(query: Option<&str>) -> Option<String> {
    let mut reason = None;
    for (key, value) in url::form_urlencoded::parse(query?.as_bytes()) {
        if key == "reason" && !value.is_empty() {
            reason = Some(value.into_owned());
        }
    }
    reason
}

fn yank_marker(query: Option<&str>) -> Yanked {
    let Some(query) = query else {
        return Yanked::Yes;
    };
    let mut reason = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if key != "reason" {
            continue;
        }
        reason = Some(value.into_owned());
    }
    reason
        .filter(|reason| !reason.is_empty())
        .map_or(Yanked::Yes, Yanked::Reason)
}

fn count_response(result: Result<usize, CacheError>) -> Response {
    match result {
        Ok(0) => (StatusCode::NOT_FOUND, "nothing to remove").into_response(),
        Ok(count) => (StatusCode::OK, format!("affected {count} file(s)")).into_response(),
        Err(err) => {
            tracing::error!(error = ?err, "removal failed");
            cache_error_response(&err, CacheContext::mutation("file removal"))
        }
    }
}

fn promotion_response(result: Result<usize, CacheError>) -> Response {
    match result {
        Ok(count) => (StatusCode::OK, format!("promoted {count} file(s)")).into_response(),
        Err(CacheError::FileExists(filename)) => (
            StatusCode::CONFLICT,
            format!("File already exists: {filename:?} has different content; use a different filename"),
        )
            .into_response(),
        Err(err) => {
            tracing::error!(error = ?err, "promotion failed");
            cache_error_response(&err, CacheContext::mutation("promotion"))
        }
    }
}
