//! Cached-package search: the `/+search` handlers and their response rendering.

use std::sync::Arc;

use axum::extract::{OriginalUri, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use peryx_driver::state::AppState;
use peryx_search::{SearchError, SearchParams};

/// `GET /{route}/+search`: search cached packages scoped to one index.
pub(super) async fn index_search(state: Arc<AppState>, position: usize, uri: &axum::http::Uri) -> Response {
    let mut params = match SearchParams::from_query(uri.query()) {
        Ok(params) => params,
        Err(err) => return search_error_response(&err),
    };
    params.route = Some(state.index_at(position).route.clone());
    search_response_offloaded(state, params).await
}

/// `GET /+search`: search cached packages across configured indexes.
pub async fn search(State(state): State<Arc<AppState>>, OriginalUri(uri): OriginalUri) -> Response {
    match SearchParams::from_query(uri.query()) {
        Ok(params) => search_response_offloaded(state, params).await,
        Err(err) => search_error_response(&err),
    }
}

/// Run a search over cached package documents and render the result document.
#[must_use]
pub fn search_response(state: &AppState, params: SearchParams) -> Response {
    match state.search.search(&state.search_ctx(), params) {
        Ok(results) => axum::Json(results).into_response(),
        Err(err) => search_error_response(&err),
    }
}

/// Run [`search_response`] on the blocking pool. A tantivy query is mmap I/O plus CPU scoring, so
/// keeping it off the async workers stops a burst of searches from stalling concurrent serving.
///
/// # Panics
/// Panics if the blocking task panics; [`search_response`] returns every error as a response, so it
/// does not.
pub async fn search_response_offloaded(state: Arc<AppState>, params: SearchParams) -> Response {
    tokio::task::spawn_blocking(move || search_response(&state, params))
        .await
        .expect("search task never panics")
}

/// Map a [`SearchError`] to a JSON error response.
#[must_use]
pub fn search_error_response(err: &SearchError) -> Response {
    let status = if err.is_bad_request() {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (status, axum::Json(serde_json::json!({ "error": err.to_string() }))).into_response()
}
