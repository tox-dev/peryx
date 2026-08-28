use std::sync::Arc;
use std::time::Duration;

use axum::Extension;
use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse as _, Response};
use axum::routing::{any, delete, get, post, put};
use http_body_util::BodyExt as _;
use tower_http::trace::{DefaultOnResponse, TraceLayer};

use crate::handlers;
use peryx_driver::http_services::HttpDomainServices;
use peryx_driver::rate_limit;
use peryx_driver::state::AppState;

/// All index traffic lands on a catch-all path that the handlers resolve to a configured index by
/// longest route prefix, so routes are data, not hardcoded. Every request is traced at info level.
pub fn router(state: Arc<AppState>) -> Router {
    let services = HttpDomainServices::for_state(&state);
    router_with_services(state, services)
}

pub fn router_with_services(state: Arc<AppState>, services: HttpDomainServices) -> Router {
    let mut router = service_routes();
    for routes in state.http_routes() {
        router = router.merge(routes.routes());
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
                .make_span_with(request_span)
                .on_response(DefaultOnResponse::new().level(tracing::Level::INFO)),
        );
    let router = if state.serving.rate_limits.enabled() {
        router.layer(middleware::from_fn_with_state(state.clone(), rate_limit::enforce))
    } else {
        router
    };
    let router = if state.serving.read_only {
        router.layer(middleware::from_fn_with_state(state.clone(), reject_replica_mutation))
    } else {
        router
    };
    router.layer(Extension(services)).with_state(state)
}

fn request_span(request: &Request) -> tracing::Span {
    let path = request.uri().path();
    if path
        .strip_prefix("/_/login/")
        .and_then(|path| path.strip_suffix("/callback"))
        .is_some_and(|provider| !provider.is_empty() && !provider.contains('/'))
    {
        return tracing::info_span!("request", method = %request.method(), uri = path, version = ?request.version());
    }
    tracing::info_span!("request", method = %request.method(), uri = %request.uri(), version = ?request.version())
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
        .route("/+stats", get(handlers::stats))
        .route("/+analytics/top-resources", get(handlers::analytics_top))
        .route("/+analytics/unused", get(handlers::analytics_unused))
        .route("/+analytics/groups", get(handlers::analytics_groups))
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
    discard_body(request.into_body());
    let body = axum::Json(serde_json::json!({
        "error": "read_only_replica",
        "message": "this replica does not accept mutations",
    }));
    match state.serving.read_only_retry_after() {
        Some(delay) => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            [(axum::http::header::RETRY_AFTER, retry_after_secs(delay).to_string())],
            body,
        )
            .into_response(),
        None => (axum::http::StatusCode::SERVICE_UNAVAILABLE, body).into_response(),
    }
}

fn retry_after_secs(delay: Duration) -> u64 {
    delay.as_secs().saturating_add(u64::from(delay.subsec_nanos() != 0))
}

fn discard_body(mut body: Body) {
    tokio::spawn(async move {
        while let Some(frame) = body.frame().await {
            if let Err(error) = frame {
                tracing::debug!(%error, "rejected request body closed before it was discarded");
                break;
            }
        }
    });
}

/// Whether a POST is a read that a read-only replica may serve: the neutral `POST /+query` surface,
/// which is read-only by construction, or a driver-classified service read.
fn is_read_only_post(state: &AppState, request: &Request) -> bool {
    let path = request.uri().path();
    if path == "/+query" || path == "/_/logout" {
        return true;
    }
    for (_, driver) in state.driver_set().services() {
        if driver
            .classify_service_post(path.trim_start_matches('/'), request.headers())
            .is_some()
        {
            return true;
        }
    }
    false
}
