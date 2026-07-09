//! Method dispatch: resolve the index by longest route prefix and hand off to its ecosystem driver.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::{Multipart, OriginalUri, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use super::discover::index_api;
use super::query::index_search;
use crate::state::AppState;

/// `GET /{route}/...`: resolve the index's ecosystem driver and let it serve the request.
///
/// `/{route}/+api` (per-index discovery) and `/{route}/+search` (cached-package search) are peryx's
/// own endpoints, not an ecosystem's wire protocol, so they are served here for every ecosystem. The
/// index is resolved once and its position handed to the driver, so a wire-protocol request pays for a
/// single route lookup.
pub async fn dispatch_get(
    State(state): State<Arc<AppState>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    state.requests.fetch_add(1, Ordering::Relaxed);
    let Some((position, rest)) = state.resolve_position(uri.path().trim_start_matches('/')) else {
        return not_found();
    };
    match rest {
        "+api" | "+api/" => index_api(&state, position, &uri, &headers),
        "+search" | "+search/" => index_search(state, position, &uri).await,
        _ if state.index_at(position).ecosystem != state.serving.ecosystem() => not_found(),
        _ => {
            let rest = rest.to_owned();
            let serving = state.serving.clone();
            serving.get(state, position, rest, uri, headers).await
        }
    }
}

/// True when the index at `path` belongs to an ecosystem the per-index serving driver does not serve
/// (a namespace ecosystem reached through the wrong route, or an unwired build). Rejecting it here
/// keeps a driver serving only indexes of its own ecosystem. An unresolvable path is not foreign; it
/// falls through to the driver, whose own lookup returns the not-found.
fn foreign_to_serving(state: &AppState, path: &str) -> bool {
    state
        .resolve_position(path)
        .is_some_and(|(position, _)| state.index_at(position).ecosystem != state.serving.ecosystem())
}

/// `POST /{route}/`: hand the upload to the index's ecosystem driver.
pub async fn dispatch_post(
    State(state): State<Arc<AppState>>,
    Path(path): Path<String>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Response {
    if foreign_to_serving(&state, &path) {
        return not_found();
    }
    let serving = state.serving.clone();
    serving.post(state, path, headers, multipart).await
}

/// `PUT /{route}/...`: hand the mutation to the index's ecosystem driver.
pub async fn dispatch_put(
    State(state): State<Arc<AppState>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    if foreign_to_serving(&state, uri.path().trim_start_matches('/')) {
        return not_found();
    }
    let serving = state.serving.clone();
    serving.put(state, uri, headers).await
}

/// `DELETE /{route}/...`: hand the mutation to the index's ecosystem driver.
pub async fn dispatch_delete(
    State(state): State<Arc<AppState>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    if foreign_to_serving(&state, uri.path().trim_start_matches('/')) {
        return not_found();
    }
    let serving = state.serving.clone();
    serving.delete(state, uri, headers).await
}

/// A `404 Not Found` with a plain body.
#[must_use]
pub fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}
