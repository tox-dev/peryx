//! The axum router.

use std::sync::Arc;

use axum::Router;
use axum::routing::get;

use crate::handlers;
use crate::state::AppState;

/// Build the velox HTTP router over the given state.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/{user}/{index}/simple/{project}/", get(handlers::simple_detail))
        .route(
            "/{user}/{index}/files/{sha256}/{filename}",
            get(handlers::file_download),
        )
        .route("/+status", get(handlers::status))
        .route("/metrics", get(handlers::metrics))
        .with_state(state)
}
