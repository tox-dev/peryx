//! The axum router.

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use tower_http::trace::TraceLayer;

use crate::handlers;
use crate::state::AppState;

/// Build the velox HTTP router over the given state. Every request is traced (method, path,
/// status) at debug level, which is how the `.metadata` fast path can be observed in the logs.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/{user}/{index}/", post(handlers::upload))
        .route("/{user}/{index}/simple/", get(handlers::simple_index))
        .route("/{user}/{index}/simple/{project}/", get(handlers::simple_detail))
        .route(
            "/{user}/{index}/files/{sha256}/{filename}",
            get(handlers::file_download),
        )
        .route("/+status", get(handlers::status))
        .route("/metrics", get(handlers::metrics))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
