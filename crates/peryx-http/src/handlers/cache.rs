//! Cache operations reuse the server's metadata-store handle because redb rejects a second open.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{Extensions, HeaderMap, Request, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use peryx_core::Ecosystem;
use peryx_driver::authz::Decision;
use peryx_driver::cache_inspection::{
    CacheListFilter, CachePageSource, resource_filter, write_cache_fsck, write_cache_list, write_cache_size,
};
use peryx_driver::serving::{CacheInspectDriver, PurgeReport};
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

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CacheListParams {
    index: Option<String>,
    resource: Option<String>,
    digest: Option<String>,
    #[serde(default)]
    stale: bool,
    min_age_secs: Option<u64>,
    min_size_bytes: Option<u64>,
}

pub async fn list_cached_contents(State(state): State<Arc<AppState>>, request: Request<Body>) -> Response {
    let (parts, _) = request.into_parts();
    if let Err(response) = administrator(&state, &parts.headers, &parts.extensions, Scope::AdministrationRead).await {
        return protected(response);
    }
    let Ok(Query(params)) = Query::<CacheListParams>::try_from_uri(&parts.uri) else {
        return protected(problem(StatusCode::BAD_REQUEST, "invalid cache list query"));
    };
    run_inspection(state, move |state| list_cache(state, &params)).await
}

pub async fn size_cached_contents(State(state): State<Arc<AppState>>, request: Request<Body>) -> Response {
    let (parts, _) = request.into_parts();
    inspect_cache(state, parts.headers, parts.extensions, size_cache).await
}

pub async fn fsck_cached_contents(State(state): State<Arc<AppState>>, request: Request<Body>) -> Response {
    let (parts, _) = request.into_parts();
    inspect_cache(state, parts.headers, parts.extensions, fsck_cache).await
}

pub async fn purge_cached_resource(State(state): State<Arc<AppState>>, request: Request<Body>) -> Response {
    protected(purge_response(&state, request).await)
}

async fn inspect_cache(
    state: Arc<AppState>,
    headers: HeaderMap,
    extensions: Extensions,
    inspect: impl FnOnce(&AppState) -> Result<Vec<u8>, String> + Send + 'static,
) -> Response {
    if let Err(response) = administrator(&state, &headers, &extensions, Scope::AdministrationRead).await {
        return protected(response);
    }
    run_inspection(state, inspect).await
}

async fn run_inspection(
    state: Arc<AppState>,
    inspect: impl FnOnce(&AppState) -> Result<Vec<u8>, String> + Send + 'static,
) -> Response {
    let blocking_scans = state.blocking_scans.clone();
    protected(match blocking_scans.run(move |_| inspect(&state)).await {
        Ok(Ok(report)) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            report,
        )
            .into_response(),
        Ok(Err(reason)) => problem(StatusCode::INTERNAL_SERVER_ERROR, &reason),
        Err(_) => problem(StatusCode::INTERNAL_SERVER_ERROR, "cache inspection task failed"),
    })
}

fn list_cache(state: &AppState, params: &CacheListParams) -> Result<Vec<u8>, String> {
    let mut sources = Vec::new();
    if params.digest.is_none() {
        let index_names = served_index_names(state);
        for (ecosystem, driver) in inspectors(state) {
            sources.push(CachePageSource {
                resource_filter: resource_filter(
                    params.resource.as_deref(),
                    state.driver_set().get_name(ecosystem).map(AsRef::as_ref),
                ),
                pages: driver
                    .served_cache_pages(&state.serving, &index_names)
                    .map_err(|reason| format!("cache list failed: scan cached index pages: {reason}"))?,
            });
        }
    }
    let mut output = Vec::new();
    write_cache_list(
        sources,
        &state.serving.blobs,
        &CacheListFilter {
            index: params.index.as_deref(),
            resource_filtered: params.resource.is_some(),
            digest: params.digest.as_deref(),
            stale: params.stale,
            min_age_secs: params.min_age_secs,
            min_size_bytes: params.min_size_bytes,
        },
        state.serving.ttl_secs,
        (state.serving.clock)(),
        &mut output,
    )
    .map_err(|error| format!("cache list failed: {error}"))?;
    Ok(output)
}

fn size_cache(state: &AppState) -> Result<Vec<u8>, String> {
    let mut pages = Vec::new();
    let mut record_counts = Vec::new();
    let index_names = served_index_names(state);
    for (_, driver) in inspectors(state) {
        pages.extend(
            driver
                .served_cache_pages(&state.serving, &index_names)
                .map_err(|reason| format!("cache size failed: scan cached index pages: {reason}"))?,
        );
        record_counts.extend(
            driver
                .served_cache_record_counts(&state.serving)
                .map_err(|reason| format!("cache size failed: {reason}"))?,
        );
    }
    let mut output = Vec::new();
    write_cache_size(
        &pages,
        record_counts,
        &state.serving.blobs,
        state.serving.ttl_secs,
        (state.serving.clock)(),
        &mut output,
    )
    .map_err(|error| format!("cache size failed: {error}"))?;
    Ok(output)
}

/// Longest first, so a resource key is attributed to the most specific index whose name prefixes it.
fn served_index_names(state: &AppState) -> Vec<&str> {
    let mut names = state
        .serving
        .indexes
        .iter()
        .map(|index| index.name.as_str())
        .collect::<Vec<_>>();
    names.sort_by_key(|name| std::cmp::Reverse(name.len()));
    names
}

/// Sorted, because the registry is a map and a report an operator diffs may not reorder between runs.
fn inspectors(state: &AppState) -> Vec<(&Ecosystem, &Arc<dyn CacheInspectDriver>)> {
    let mut drivers = state.driver_set().cache_inspect_drivers().collect::<Vec<_>>();
    drivers.sort_unstable_by_key(|(ecosystem, _)| ecosystem.as_str());
    drivers
}

fn fsck_cache(state: &AppState) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    write_cache_fsck(
        state.driver_set(),
        &state.serving.meta,
        &state.serving.blobs,
        &mut output,
    )
    .map_err(|error| format!("cache fsck failed: {error}"))?;
    Ok(output)
}

fn protected(mut response: Response) -> Response {
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
