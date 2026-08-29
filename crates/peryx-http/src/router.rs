use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Extension;
use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, MatchedPath, Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse as _, Response};
use axum::routing::{any, delete, get, post, put};
use http_body_util::BodyExt as _;
use tower_http::trace::{DefaultOnResponse, TraceLayer};

use crate::handlers;
use peryx_driver::http_services::HttpDomainServices;
use peryx_driver::rate_limit;
use peryx_driver::state::AppState;
use peryx_driver::{
    ProcessRouteMethodNotAllowed, RouteDescriptor, RouteMethod, RoutePosture, RouteRateLimit, RouteSet,
};

/// All index traffic lands on a catch-all path that the handlers resolve to a configured index by
/// longest route prefix, so routes are data, not hardcoded. Every request is traced at info level.
pub fn router(state: Arc<AppState>) -> Router {
    let services = HttpDomainServices::for_state(&state);
    router_with_services(state, services)
}

pub fn router_with_services(state: Arc<AppState>, services: HttpDomainServices) -> Router {
    let mut route_set = service_routes();
    for registered in state.http_routes() {
        route_set = route_set.merge(registered.routes());
    }
    let (mut router, descriptors) = route_set.into_parts();
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
    let classify_routes = state.serving.rate_limits.enabled() || state.serving.read_only;
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
    let router = if classify_routes {
        router.layer(middleware::from_fn_with_state(
            Arc::new(RouteRegistry::new(descriptors)),
            attach_route_descriptor,
        ))
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

fn service_routes() -> RouteSet {
    public_routes()
        .merge(authentication_routes())
        .merge(analytics_routes())
        .merge(repository_routes())
        .merge(security_routes())
}

fn public_routes() -> RouteSet {
    RouteSet::new()
        .route(
            read(
                RouteMethod::Get,
                "/api-docs/openapi.json",
                rate_limit::RouteClass::Admin,
            ),
            get(handlers::openapi_spec),
        )
        .route(
            read(RouteMethod::Get, "/+api", rate_limit::RouteClass::Admin),
            get(handlers::api),
        )
        .route(
            read(RouteMethod::Get, "/+api/", rate_limit::RouteClass::Admin),
            get(handlers::api),
        )
        .route(
            read(RouteMethod::Get, "/+search", rate_limit::RouteClass::Listing),
            get(handlers::search),
        )
        .route(
            read(RouteMethod::Get, "/+search/", rate_limit::RouteClass::Listing),
            get(handlers::search),
        )
        .route(
            read(RouteMethod::Get, "/+status", rate_limit::RouteClass::Admin),
            get(handlers::status),
        )
        .route(exempt_read(RouteMethod::Get, "/+health"), get(handlers::health))
        .route(exempt_read(RouteMethod::Get, "/+ready"), get(handlers::readiness))
        .route(
            read(RouteMethod::Get, "/+acl", rate_limit::RouteClass::Admin),
            get(handlers::acl),
        )
}

fn authentication_routes() -> RouteSet {
    RouteSet::new()
        .route(
            read(
                RouteMethod::Get,
                "/_/login/{provider}",
                rate_limit::RouteClass::Authentication,
            ),
            get(handlers::login_start),
        )
        .route(
            mutation(
                RouteMethod::Get,
                "/_/login/{provider}/callback",
                rate_limit::RouteClass::Authentication,
            ),
            get(handlers::login_callback),
        )
        .route(
            read(RouteMethod::Post, "/_/logout", rate_limit::RouteClass::Authentication),
            post(handlers::logout),
        )
        .route(
            read(RouteMethod::Get, "/_/session", rate_limit::RouteClass::Authentication),
            get(handlers::session),
        )
}

fn analytics_routes() -> RouteSet {
    RouteSet::new()
        .route(
            read(RouteMethod::Get, "/+stats", rate_limit::RouteClass::Admin),
            get(handlers::stats),
        )
        .route(
            read(
                RouteMethod::Get,
                "/+analytics/top-resources",
                rate_limit::RouteClass::Admin,
            ),
            get(handlers::analytics_top),
        )
        .route(
            read(RouteMethod::Get, "/+analytics/unused", rate_limit::RouteClass::Admin),
            get(handlers::analytics_unused),
        )
        .route(
            read(RouteMethod::Get, "/+analytics/groups", rate_limit::RouteClass::Admin),
            get(handlers::analytics_groups),
        )
        .route(
            read(RouteMethod::Get, "/+analytics/sources", rate_limit::RouteClass::Admin),
            get(handlers::analytics_sources),
        )
        .route(
            read(RouteMethod::Get, "/+analytics/timeline", rate_limit::RouteClass::Admin),
            get(handlers::analytics_timeline),
        )
        .route(
            read(RouteMethod::Get, "/+policy/decisions", rate_limit::RouteClass::Admin),
            get(handlers::policy_decisions),
        )
        .route(
            read(RouteMethod::Post, "/+query", rate_limit::RouteClass::Admin),
            post(handlers::pql_query).layer(DefaultBodyLimit::max(16 * 1024)),
        )
        .route(
            read(RouteMethod::Get, "/+quota", rate_limit::RouteClass::Admin),
            get(handlers::quota_summary),
        )
        .route(
            read(RouteMethod::Get, "/+quota/repository", rate_limit::RouteClass::Admin),
            get(handlers::quota_repository),
        )
}

fn repository_routes() -> RouteSet {
    RouteSet::new()
        .route(
            read(RouteMethod::Get, "/+repositories", rate_limit::RouteClass::Admin),
            get(handlers::list_repositories),
        )
        .route(
            mutation(RouteMethod::Post, "/+repositories", rate_limit::RouteClass::Admin),
            post(handlers::create_repository).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            read(RouteMethod::Get, "/+repositories/{id}", rate_limit::RouteClass::Admin),
            get(handlers::inspect_repository),
        )
        .route(
            mutation(RouteMethod::Put, "/+repositories/{id}", rate_limit::RouteClass::Admin),
            put(handlers::update_repository).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            mutation(
                RouteMethod::Post,
                "/+repositories/{id}/disable",
                rate_limit::RouteClass::Admin,
            ),
            post(handlers::disable_repository),
        )
        .route(
            mutation(
                RouteMethod::Post,
                "/+repositories/{id}/enable",
                rate_limit::RouteClass::Admin,
            ),
            post(handlers::enable_repository),
        )
        .route(
            mutation(RouteMethod::Post, "/+retention/plan", rate_limit::RouteClass::Admin),
            post(handlers::retention_plan),
        )
        .route(
            mutation(RouteMethod::Post, "/+retention/export", rate_limit::RouteClass::Admin),
            post(handlers::retention_export),
        )
        .route(
            read(RouteMethod::Get, "/+trash", rate_limit::RouteClass::Admin),
            get(handlers::list_trash),
        )
        .route(
            read(RouteMethod::Get, "/+trash/record", rate_limit::RouteClass::Admin),
            get(handlers::inspect_trash),
        )
}

fn security_routes() -> RouteSet {
    RouteSet::new()
        .route(
            read(RouteMethod::Get, "/+revocations", rate_limit::RouteClass::Admin),
            get(handlers::list_revocations),
        )
        .route(
            read(
                RouteMethod::Get,
                "/+revocations/{digest}",
                rate_limit::RouteClass::Admin,
            ),
            get(handlers::inspect_revocation),
        )
        .route(
            mutation(
                RouteMethod::Put,
                "/+revocations/{digest}",
                rate_limit::RouteClass::Admin,
            ),
            put(handlers::put_revocation),
        )
        .route(
            mutation(
                RouteMethod::Post,
                "/+revocations/{digest}/lift",
                rate_limit::RouteClass::Admin,
            ),
            post(handlers::lift_revocation),
        )
        .route(
            read(RouteMethod::Get, "/+grants", rate_limit::RouteClass::Admin),
            get(handlers::list_grants),
        )
        .route(
            mutation(RouteMethod::Post, "/+grants", rate_limit::RouteClass::Admin),
            post(handlers::create_grant),
        )
        .route(
            read(RouteMethod::Get, "/+grants/{id}", rate_limit::RouteClass::Admin),
            get(handlers::inspect_grant),
        )
        .route(
            mutation(RouteMethod::Delete, "/+grants/{id}", rate_limit::RouteClass::Admin),
            delete(handlers::revoke_grant),
        )
        .route(
            read(RouteMethod::Get, "/+tokens", rate_limit::RouteClass::Admin),
            get(handlers::list_tokens),
        )
        .route(
            mutation(RouteMethod::Post, "/+tokens", rate_limit::RouteClass::Admin),
            post(handlers::create_token),
        )
        .route(
            read(RouteMethod::Get, "/+tokens/{id}", rate_limit::RouteClass::Admin),
            get(handlers::inspect_token),
        )
        .route(
            mutation(RouteMethod::Delete, "/+tokens/{id}", rate_limit::RouteClass::Admin),
            delete(handlers::revoke_token),
        )
        .route(
            mutation(RouteMethod::Post, "/+tokens/{id}/rotate", rate_limit::RouteClass::Admin),
            post(handlers::rotate_token),
        )
        .route(
            mutation(RouteMethod::Post, "/+jobs/{id}/cancel", rate_limit::RouteClass::Admin),
            post(handlers::cancel_job),
        )
        .route(
            read(RouteMethod::Get, "/metrics", rate_limit::RouteClass::Admin),
            get(handlers::metrics),
        )
}

const fn read(method: RouteMethod, path: &'static str, class: rate_limit::RouteClass) -> RouteDescriptor {
    RouteDescriptor::new(method, path, RoutePosture::Read, RouteRateLimit::Class(class))
}

const fn mutation(method: RouteMethod, path: &'static str, class: rate_limit::RouteClass) -> RouteDescriptor {
    RouteDescriptor::new(method, path, RoutePosture::Mutation, RouteRateLimit::Class(class))
}

const fn exempt_read(method: RouteMethod, path: &'static str) -> RouteDescriptor {
    RouteDescriptor::new(method, path, RoutePosture::Read, RouteRateLimit::Exempt)
}

struct RouteRegistry {
    by_path: HashMap<&'static str, Vec<RouteDescriptor>>,
}

impl RouteRegistry {
    fn new(descriptors: Vec<RouteDescriptor>) -> Self {
        let mut by_path: HashMap<_, Vec<_>> = HashMap::new();
        for descriptor in descriptors {
            by_path.entry(descriptor.path()).or_default().push(descriptor);
        }
        Self { by_path }
    }

    fn get(&self, path: &str, method: &axum::http::Method) -> Option<RouteDescriptor> {
        self.by_path
            .get(path)
            .and_then(|descriptors| {
                descriptors
                    .iter()
                    .find(|descriptor| descriptor.method().matches(method))
            })
            .copied()
    }

    fn contains(&self, path: &str) -> bool {
        self.by_path.contains_key(path)
    }
}

async fn attach_route_descriptor(
    State(registry): State<Arc<RouteRegistry>>,
    matched_path: MatchedPath,
    mut request: Request,
    next: Next,
) -> Response {
    let path = matched_path.as_str();
    if let Some(descriptor) = registry.get(path, request.method()) {
        request.extensions_mut().insert(descriptor);
    } else if registry.contains(path) {
        request.extensions_mut().insert(ProcessRouteMethodNotAllowed);
    }
    next.run(request).await
}

async fn reject_replica_mutation(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    if request.extensions().get::<ProcessRouteMethodNotAllowed>().is_some() {
        return next.run(request).await;
    }
    if let Some(descriptor) = request.extensions().get::<RouteDescriptor>() {
        if descriptor.posture() == RoutePosture::Read {
            return next.run(request).await;
        }
        return reject_mutation(&state, request);
    }
    if matches!(
        *request.method(),
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    ) || (*request.method() == axum::http::Method::POST && is_read_only_post(&state, &request))
    {
        return next.run(request).await;
    }
    reject_mutation(&state, request)
}

fn reject_mutation(state: &AppState, request: Request) -> Response {
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

/// Whether an ecosystem service driver classifies a POST as a read.
fn is_read_only_post(state: &AppState, request: &Request) -> bool {
    let path = request.uri().path();
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

#[cfg(test)]
mod route_tests {
    use super::service_routes;
    use peryx_driver::rate_limit::RouteClass;
    use peryx_driver::{RoutePosture, RouteRateLimit};

    #[test]
    fn process_routes_declare_complete_semantics() {
        let (_, descriptors) = service_routes().into_parts();

        assert_eq!(descriptors.len(), 48);
        assert_eq!(
            descriptors
                .iter()
                .filter(|descriptor| descriptor.posture() == RoutePosture::Mutation)
                .count(),
            15
        );
        assert_eq!(
            [
                RouteRateLimit::Exempt,
                RouteRateLimit::Class(RouteClass::Listing),
                RouteRateLimit::Class(RouteClass::Admin),
                RouteRateLimit::Class(RouteClass::Authentication),
            ]
            .map(|rate_limit| {
                descriptors
                    .iter()
                    .filter(|descriptor| descriptor.rate_limit() == rate_limit)
                    .count()
            }),
            [2, 2, 40, 4]
        );
    }
}
