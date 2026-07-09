//! API discovery: the neutral envelope plus each index's driver-rendered entry, and the `OpenAPI` doc.

use std::sync::Arc;

use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};

use crate::state::AppState;

/// `GET /{route}/+api`: the discovery entry for one index, rendered by its ecosystem driver and
/// wrapped in the neutral envelope.
pub(super) fn index_api(state: &AppState, position: usize, uri: &axum::http::Uri, headers: &HeaderMap) -> Response {
    let base = crate::discovery::BaseUrl::from_request(headers, uri);
    let description = crate::state::describe_index(&state.indexes, position);
    let entry = discover_index_entry(state, description, base.as_ref());
    axum::Json(crate::discovery::index_envelope(entry)).into_response()
}

/// The `/+api` entry for one index, rendered by whichever driver serves its ecosystem: a namespace
/// driver when one claims the ecosystem, else the per-index driver, else a minimal entry for an
/// ecosystem no installed driver serves.
fn discover_index_entry(
    state: &AppState,
    index: crate::state::IndexDescription,
    base: Option<&crate::discovery::BaseUrl>,
) -> serde_json::Value {
    if let Some(driver) = state.namespace_for_ecosystem(index.ecosystem) {
        driver.discover_index(index, base)
    } else if state.serving.ecosystem().as_str() == index.ecosystem {
        state.serving.discover_index(index, base)
    } else {
        crate::discovery::minimal_entry(&index)
    }
}

/// `GET /api-docs/openapi.json`: the `OpenAPI` description of this server.
pub async fn openapi_spec(State(state): State<Arc<AppState>>) -> Response {
    ([(header::CONTENT_TYPE, "application/json")], state.openapi().to_owned()).into_response()
}

/// `GET /+api`: API discovery and copyable client configuration.
///
/// The envelope (version, service URLs) is neutral; each configured index's entry is rendered by its
/// own ecosystem driver, so the document covers every ecosystem the server hosts.
pub async fn api(State(state): State<Arc<AppState>>, OriginalUri(uri): OriginalUri, headers: HeaderMap) -> Response {
    let base = crate::discovery::BaseUrl::from_request(&headers, &uri);
    let indexes = state
        .describe_indexes()
        .into_iter()
        .map(|index| discover_index_entry(&state, index, base.as_ref()))
        .collect();
    axum::Json(crate::discovery::root_envelope(base.as_ref(), indexes)).into_response()
}
