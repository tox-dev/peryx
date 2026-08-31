//! Purging a cached resource without stopping the server. The metadata store is exclusive, so the
//! `peryx cache purge` path is only available while nothing is serving; this is the same removal
//! driven in-process, fenced by the driver against its own cache writers.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Extensions, HeaderMap, Request, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use peryx_driver::authz::Decision;
use peryx_driver::serving::PurgeReport;
use peryx_driver::state::AppState;
use peryx_identity::{Resource, Scope, parse_basic};

use crate::response_security::ProtectedCachePolicy;

const MAX_BODY_BYTES: usize = 8 * 1024;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PurgeRequest {
    repository: String,
    resource: String,
    /// Delete the planned records; omission previews them, matching `peryx cache purge --yes`.
    #[serde(default)]
    apply: bool,
}

#[derive(serde::Serialize)]
struct PurgeResponse {
    repository: String,
    resource: String,
    applied: bool,
    /// Driver-owned record categories mapped to the rows removed, or to the rows a preview counted.
    removed: BTreeMap<String, u64>,
}

pub async fn purge_cached_resource(State(state): State<Arc<AppState>>, request: Request<Body>) -> Response {
    let mut response = purge_response(&state, request).await;
    ProtectedCachePolicy::NoStore.apply(response.headers_mut());
    response
}

async fn purge_response(state: &AppState, request: Request<Body>) -> Response {
    let (parts, body) = request.into_parts();
    if !super::is_json(&parts.headers) {
        return problem(StatusCode::UNSUPPORTED_MEDIA_TYPE, "request body must be JSON");
    }
    let Ok(body) = axum::body::to_bytes(body, MAX_BODY_BYTES).await else {
        return problem(StatusCode::PAYLOAD_TOO_LARGE, "request body is too large");
    };
    let Ok(request) = serde_json::from_slice::<PurgeRequest>(&body) else {
        return problem(StatusCode::UNPROCESSABLE_ENTITY, "invalid request body");
    };
    // A preview writes nothing, so it asks only for the read scope the equivalent CLI dry run needs.
    let scope = if request.apply {
        Scope::AdministrationWrite
    } else {
        Scope::AdministrationRead
    };
    if let Err(rejection) = administrator(state, &parts.headers, &parts.extensions, scope).await {
        return rejection;
    }
    // A repository the caller cannot resolve is a 404 rather than a distinct error, so an
    // administrator cannot probe which repositories exist by the shape of the failure.
    let Some(index) = super::index_by_route(state, &request.repository) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(driver) = state.driver_set().get_cache_purge(&index.ecosystem) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let name = index.name.clone();
    let purge = driver
        .purge_served_resource(state.serving.clone(), &name, &request.resource, request.apply)
        .await;
    match purge {
        Ok(report) => purged(name, request.apply, report),
        Err(reason) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("cache purge failed: {reason}"),
        ),
    }
}

fn purged(repository: String, applied: bool, report: PurgeReport) -> Response {
    axum::Json(PurgeResponse {
        repository,
        resource: report.resource,
        applied,
        removed: report.categories.into_iter().collect(),
    })
    .into_response()
}

async fn administrator(
    state: &AppState,
    headers: &HeaderMap,
    extensions: &Extensions,
    scope: Scope,
) -> Result<(), Response> {
    let credentials = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_basic)
        .ok_or_else(unauthorized)?;
    let actor = state
        .serving
        .users
        .authenticate_request(extensions, &credentials)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(unauthorized)?;
    let decision = state
        .serving
        .authorization
        .authorize_scoped(&actor, scope, &Resource::Operator);
    if decision.decision() == Decision::Allow {
        return Ok(());
    }
    Err(StatusCode::NOT_FOUND.into_response())
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"peryx-administration\"")],
    )
        .into_response()
}

fn unavailable() -> Response {
    problem(StatusCode::SERVICE_UNAVAILABLE, "user directory unavailable")
}

fn problem(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(serde_json::json!({"error": message}))).into_response()
}
