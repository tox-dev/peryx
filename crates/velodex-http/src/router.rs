//! The axum router.

use std::sync::Arc;

use axum::Router;
use axum::middleware;
use axum::routing::get;
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};

use crate::handlers;
use crate::rate_limit;
use crate::state::AppState;

/// Build the velodex HTTP router.
///
/// All index traffic lands on a catch-all path that the handlers resolve to a configured index by
/// longest route prefix, so routes are data, not hardcoded. Every request is traced (method, path,
/// status) at info level, so the default log level already shows the `.metadata` fast path.
pub fn router(state: Arc<AppState>) -> Router {
    let router = Router::new()
        .route("/api-docs/openapi.json", get(handlers::openapi_spec))
        .route("/+api", get(handlers::api))
        .route("/+api/", get(handlers::api))
        .route("/+search", get(handlers::search))
        .route("/+search/", get(handlers::search))
        .route("/+status", get(handlers::status))
        .route("/+stats", get(handlers::stats))
        .route("/metrics", get(handlers::metrics))
        .route(
            "/{*path}",
            get(handlers::dispatch_get)
                .post(handlers::dispatch_post)
                .put(handlers::dispatch_put)
                .delete(handlers::dispatch_delete),
        )
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(DefaultMakeSpan::new().level(tracing::Level::INFO))
                .on_response(DefaultOnResponse::new().level(tracing::Level::INFO)),
        );
    let router = if state.rate_limits.enabled() {
        router.layer(middleware::from_fn_with_state(state.clone(), rate_limit::enforce))
    } else {
        router
    };
    router.with_state(state)
}
