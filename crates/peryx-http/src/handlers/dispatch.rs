//! Method dispatch: resolve the index by longest route prefix and hand off to its ecosystem driver.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::{Multipart, OriginalUri, Path, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use super::discover::{index_api, trusts_proxy};
use super::query::index_search;
use peryx_driver::serving::EcosystemDriver;
use peryx_driver::state::AppState;

/// Why a request reached no driver.
enum NoDriver {
    /// No index owns the path, or the index's ecosystem serves under its own top-level prefix
    /// (`OCI`'s `/v2/`) and so is not reachable through its per-index route.
    Unroutable,
    /// Nothing was wired in at all. That is a build fault, not a missing index, so it says so.
    Unconfigured,
}

impl NoDriver {
    fn response(self) -> Response {
        match self {
            Self::Unroutable => not_found(),
            Self::Unconfigured => (StatusCode::SERVICE_UNAVAILABLE, "no ecosystem driver configured").into_response(),
        }
    }
}

/// The driver for the index already resolved to `position`.
fn driver_at(state: &AppState, position: usize) -> Result<&Arc<dyn EcosystemDriver>, NoDriver> {
    state.driver_for(state.index_at(position).ecosystem).ok_or_else(|| {
        if state.has_any_driver() {
            NoDriver::Unroutable
        } else {
            NoDriver::Unconfigured
        }
    })
}

/// The driver serving the index `path` resolves to. Used by the write methods, which have not already
/// resolved the route; `GET` resolves once and calls [`driver_at`] instead.
fn driver_for<'a>(state: &'a AppState, path: &str) -> Result<&'a Arc<dyn EcosystemDriver>, NoDriver> {
    let Some((position, _)) = state.resolve_position(path) else {
        return Err(NoDriver::Unroutable);
    };
    driver_at(state, position)
}

/// `GET /{route}/...`: resolve the index's ecosystem driver and let it serve the request.
///
/// `/{route}/+api` (per-index discovery) and `/{route}/+search` (cached-package search) are peryx's
/// own endpoints, not an ecosystem's wire protocol, so they are served here for every ecosystem. The
/// index is resolved once and its position handed to the driver, so a wire-protocol request pays for a
/// single route lookup.
///
/// axum routes a `HEAD` to the `GET` handler and strips the body from what comes back, so the method
/// travels to the driver: only the driver can answer a `HEAD` without first producing bytes nobody reads.
pub async fn dispatch_get(
    State(state): State<Arc<AppState>>,
    OriginalUri(uri): OriginalUri,
    request: Request,
) -> Response {
    state.requests.fetch_add(1, Ordering::Relaxed);
    let Some((position, rest)) = state.resolve_position(uri.path().trim_start_matches('/')) else {
        return not_found();
    };
    let trusted_proxy = trusts_proxy(&state, &request);
    let (parts, _) = request.into_parts();
    match rest {
        "+api" | "+api/" => index_api(&state, position, &uri, &parts.headers, trusted_proxy),
        "+search" | "+search/" => index_search(state, position, &uri, &parts.headers).await,
        _ => {
            let serving = match driver_at(&state, position) {
                Ok(serving) => serving.clone(),
                Err(reason) => return reason.response(),
            };
            let rest = rest.to_owned();
            serving
                .get(state.serving.clone(), position, rest, uri, parts.headers, parts.method)
                .await
        }
    }
}

/// `POST /{route}/`: hand the upload to the index's ecosystem driver.
pub async fn dispatch_post(
    State(state): State<Arc<AppState>>,
    Path(path): Path<String>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Response {
    let serving = match driver_for(&state, &path) {
        Ok(serving) => serving.clone(),
        Err(reason) => return reason.response(),
    };
    serving.post(state.serving.clone(), path, headers, multipart).await
}

/// `PUT /{route}/...`: hand the mutation to the index's ecosystem driver.
pub async fn dispatch_put(
    State(state): State<Arc<AppState>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    let serving = match driver_for(&state, uri.path().trim_start_matches('/')) {
        Ok(serving) => serving.clone(),
        Err(reason) => return reason.response(),
    };
    serving.put(state.serving.clone(), uri, headers).await
}

/// `DELETE /{route}/...`: hand the mutation to the index's ecosystem driver.
pub async fn dispatch_delete(
    State(state): State<Arc<AppState>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    let serving = match driver_for(&state, uri.path().trim_start_matches('/')) {
        Ok(serving) => serving.clone(),
        Err(reason) => return reason.response(),
    };
    serving.delete(state.serving.clone(), uri, headers).await
}

/// A `404 Not Found` with a plain body.
#[must_use]
pub fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}
