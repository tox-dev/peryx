//! The axum router.

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse as _, Response};
use axum::routing::{any, delete, get, post, put};
use axum::{Extension, Router};
use tower_http::trace::{DefaultMakeSpan, DefaultOnResponse, TraceLayer};

use crate::handlers;
use peryx_driver::rate_limit;
use peryx_driver::state::AppState;

/// Build the peryx HTTP router.
///
/// All index traffic lands on a catch-all path that the handlers resolve to a configured index by
/// longest route prefix, so routes are data, not hardcoded. Every request is traced (method, path,
/// status) at info level, so the default log level already shows the `.metadata` fast path.
pub fn router(state: Arc<AppState>) -> Router {
    let mut router = service_routes();
    if let Some(runtime) = &state.trusted_publishing {
        router = router.merge(
            Router::new()
                .route("/_/oidc/audience", get(handlers::oidc_audience))
                .route(
                    "/_/oidc/mint-token",
                    post(handlers::oidc_mint_token).layer(DefaultBodyLimit::max(40 * 1024)),
                )
                .layer(Extension(runtime.clone())),
        );
    }
    // An absolute-mount ecosystem owns the top-level prefixes it declares; mount a catch-all under
    // each, bound to that driver, so the router reaches it without naming the ecosystem.
    for (prefix, driver) in state.absolute_mounts() {
        let driver = driver.clone();
        let serve = move |State(state): State<Arc<AppState>>, request: Request| {
            let driver = driver.clone();
            async move { driver.serve(state.serving.clone(), request).await }
        };
        router = router
            .route(prefix, any(serve.clone()))
            .route(&format!("{prefix}{{*rest}}"), any(serve));
    }
    let router = router
        .route(
            "/{*path}",
            get(handlers::dispatch_get)
                .put(handlers::dispatch_put)
                .delete(handlers::dispatch_delete)
                .merge(post(handlers::dispatch_post).layer(DefaultBodyLimit::disable())),
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
    let router = if state.read_only {
        router.layer(middleware::from_fn_with_state(state.clone(), reject_replica_mutation))
    } else {
        router
    };
    router.with_state(state)
}

fn service_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api-docs/openapi.json", get(handlers::openapi_spec))
        .route("/+api", get(handlers::api))
        .route("/+api/", get(handlers::api))
        .route("/+search", get(handlers::search))
        .route("/+search/", get(handlers::search))
        .route("/+status", get(handlers::status))
        .route("/+health", get(handlers::health))
        .route("/+ready", get(handlers::readiness))
        .route("/+acl", get(handlers::acl))
        .route("/_/login/{provider}", get(handlers::login_start))
        .route("/_/login/{provider}/callback", get(handlers::login_callback))
        .route("/_/logout", post(handlers::logout))
        .route("/_/session", get(handlers::session))
        .route("/+availability/topology", get(handlers::availability_topology))
        .route(
            "/+availability/topology/stream",
            get(handlers::availability_topology_stream),
        )
        .route("/+availability/operations", get(handlers::operations))
        .route("/+availability/placements", get(handlers::placements))
        .route("/+availability/placements/{digest}", get(handlers::blob_placements))
        .route("/+stats", get(handlers::stats))
        .route("/+analytics/completeness", get(handlers::analytics_completeness))
        .route("/+analytics/top-packages", get(handlers::analytics_top))
        .route("/+analytics/unused", get(handlers::analytics_unused))
        .route("/+analytics/versions", get(handlers::analytics_versions))
        .route("/+analytics/sources", get(handlers::analytics_sources))
        .route("/+analytics/timeline", get(handlers::analytics_timeline))
        .route("/+policy/decisions", get(handlers::policy_decisions))
        .route(
            "/+query",
            post(handlers::pql_query).layer(DefaultBodyLimit::max(16 * 1024)),
        )
        .route("/+quota", get(handlers::quota_summary))
        .route("/+quota/repository", get(handlers::quota_repository))
        .route(
            "/+repositories",
            get(handlers::list_repositories)
                .merge(post(handlers::create_repository).layer(DefaultBodyLimit::max(64 * 1024))),
        )
        .route(
            "/+repositories/{id}",
            get(handlers::inspect_repository)
                .merge(put(handlers::update_repository).layer(DefaultBodyLimit::max(64 * 1024))),
        )
        .route("/+repositories/{id}/disable", post(handlers::disable_repository))
        .route("/+repositories/{id}/enable", post(handlers::enable_repository))
        .route("/+shadow/candidates", get(handlers::shadow_candidates))
        .route("/+retention/plan", post(handlers::retention_plan))
        .route("/+retention/export", post(handlers::retention_export))
        .route("/+trash", get(handlers::list_trash))
        .route("/+trash/record", get(handlers::inspect_trash))
        .route("/+revocations", get(handlers::list_revocations))
        .route(
            "/+revocations/{digest}",
            get(handlers::inspect_revocation).merge(put(handlers::put_revocation)),
        )
        .route("/+revocations/{digest}/lift", post(handlers::lift_revocation))
        .route(
            "/+grants",
            get(handlers::list_grants).merge(post(handlers::create_grant)),
        )
        .route(
            "/+grants/{id}",
            get(handlers::inspect_grant).merge(delete(handlers::revoke_grant)),
        )
        .route(
            "/+tokens",
            post(handlers::create_token).merge(get(handlers::list_tokens)),
        )
        .route(
            "/+tokens/{id}",
            get(handlers::inspect_token).merge(axum::routing::delete(handlers::revoke_token)),
        )
        .route("/+tokens/{id}/rotate", post(handlers::rotate_token))
        .route("/+jobs/{id}/cancel", post(handlers::cancel_job))
        .route("/+ui/projects", get(handlers::ui_projects))
        .route("/+ui/project", get(handlers::ui_project))
        .route("/+ui/manifest", get(handlers::ui_manifest))
        .route("/+ui/members", get(handlers::ui_members))
        .route("/+ui/member", get(handlers::ui_member))
        .route("/metrics", get(handlers::metrics))
}

async fn reject_replica_mutation(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    if matches!(
        *request.method(),
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    ) || (*request.method() == axum::http::Method::POST && is_read_only_post(&state, &request))
    {
        return next.run(request).await;
    }
    (
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(serde_json::json!({
            "error": "read_only_replica",
            "message": "this replica does not accept mutations",
        })),
    )
        .into_response()
}

/// Whether a POST is a read that a read-only replica may serve: the neutral `POST /+query` surface,
/// which is read-only by construction, or a driver-classified service read.
fn is_read_only_post(state: &AppState, request: &Request) -> bool {
    let path = request.uri().path();
    path == "/+query"
        || path == "/_/logout"
        || state.drivers().any(|driver| {
            driver.capabilities().service.is_some_and(|driver| {
                driver
                    .classify_service_post(path.trim_start_matches('/'), request.headers())
                    .is_some()
            })
        })
}
