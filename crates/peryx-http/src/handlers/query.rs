use std::sync::Arc;

use axum::Extension;
use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};

use peryx_driver::access::ReadAccess;
use peryx_driver::authz::Decision;
use peryx_driver::http_services::HttpDomainServices;
use peryx_driver::state::{AppState, Index};
use peryx_identity::{Resource, Scope, parse_basic};
use peryx_search::{SearchAccess, SearchError, SearchParams};

use crate::response_security::ProtectedCachePolicy;

pub(super) async fn index_search(
    state: Arc<AppState>,
    services: HttpDomainServices,
    position: usize,
    uri: &axum::http::Uri,
    headers: &HeaderMap,
) -> Response {
    let mut params = match SearchParams::from_query(uri.query()) {
        Ok(params) => params,
        Err(err) => return search_error_response(&err),
    };
    params.route = Some(state.serving.index_at(position).route.clone());
    params.pattern_authority = pattern_authority(&state, &params, headers).await;
    let access = search_access(&state, headers, std::slice::from_ref(state.serving.index_at(position)));
    search_response_offloaded(services, params, access).await
}

pub async fn search(
    State(state): State<Arc<AppState>>,
    Extension(services): Extension<HttpDomainServices>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    match SearchParams::from_query(uri.query()) {
        Ok(mut params) => {
            params.pattern_authority = pattern_authority(&state, &params, &headers).await;
            let access = search_access(&state, &headers, &state.serving.indexes);
            search_response_offloaded(services, params, access).await
        }
        Err(err) => search_error_response(&err),
    }
}

/// A pattern query has no prefilter to seek into, so it reads every indexed document and stays
/// operator-only. Only a pattern query pays for authentication; ordinary search keeps answering
/// without a credential round trip.
async fn pattern_authority(state: &AppState, params: &SearchParams, headers: &HeaderMap) -> bool {
    params.is_pattern() && operator_reads(state, headers).await
}

async fn operator_reads(state: &AppState, headers: &HeaderMap) -> bool {
    let Some(credentials) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_basic)
    else {
        return false;
    };
    let Ok(Some(actor)) = state
        .serving
        .users
        .authenticate(&credentials.user, &credentials.password)
        .await
    else {
        return false;
    };
    matches!(
        state
            .serving
            .authorization
            .authorize_scoped(&actor, Scope::OperatorRead, &Resource::Operator)
            .decision(),
        Decision::Allow
    )
}

fn search_access(state: &AppState, headers: &HeaderMap, indexes: &[Index]) -> Option<SearchAccess> {
    if indexes.iter().all(|index| index.acl.anonymous_read) {
        return None;
    }
    Some(ReadAccess::for_request(&state.serving, headers).search_access(indexes))
}

/// Run [`search_response`] on the blocking pool. A tantivy query is mmap I/O plus CPU scoring, so
/// keeping it off the async workers stops a burst of searches from stalling concurrent serving.
///
/// # Panics
/// Panics if the blocking task panics; [`search_response`] returns every error as a response, so it
/// does not.
pub async fn search_response_offloaded(
    services: HttpDomainServices,
    params: SearchParams,
    access: Option<SearchAccess>,
) -> Response {
    tokio::task::spawn_blocking(move || search_response(&services, params, access.as_ref()))
        .await
        .expect("search task never panics")
}

#[must_use]
pub fn search_response(services: &HttpDomainServices, params: SearchParams, access: Option<&SearchAccess>) -> Response {
    let mut response = match services.search().search(params, access) {
        Ok(results) => axum::Json(results).into_response(),
        Err(err) => search_error_response(&err),
    };
    if access.is_some() {
        ProtectedCachePolicy::NoStore.apply(response.headers_mut());
    }
    response
}

#[must_use]
pub fn search_error_response(err: &SearchError) -> Response {
    let status = if err.is_forbidden() {
        StatusCode::FORBIDDEN
    } else if err.is_bad_request() {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (status, axum::Json(serde_json::json!({ "error": err.to_string() }))).into_response()
}
