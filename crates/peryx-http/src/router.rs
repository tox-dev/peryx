use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::Extension;
use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, MatchedPath, OriginalUri, Request, State};
use axum::http::Uri;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse as _, Response};
use axum::routing::{any, delete, get, post, put};
use http_body_util::BodyExt as _;
use tower::{ServiceBuilder, util::MapRequestLayer};
use tower_http::trace::{DefaultOnResponse, TraceLayer};

use crate::handlers;
use peryx_driver::client_address;
use peryx_driver::http_services::HttpDomainServices;
use peryx_driver::rate_limit;
use peryx_driver::state::AppState;
use peryx_driver::{
    MountedRoutes, ProcessRouteMethodNotAllowed, RouteDescriptor, RouteMethod, RoutePosture, RouteRateLimit, RouteSet,
};

/// All index traffic lands on a catch-all path that the handlers resolve to a configured index by
/// longest route prefix, so routes are data, not hardcoded. Every request is traced at info level.
pub fn router(state: Arc<AppState>) -> Router {
    let services = HttpDomainServices::for_state(&state);
    router_with_services(state, services)
}

pub fn router_with_services(state: Arc<AppState>, services: HttpDomainServices) -> Router {
    router_with_ui(state, services, MountedRoutes::default(), Router::new())
}

/// Composes the whole request surface: service routes, server-rendered pages, and the routes that
/// answer outside request accounting.
///
/// The pages join the service routes before any layer attaches, because axum applies a layer to the
/// routes registered at the point of the call: merging them afterwards would leave the pages outside
/// tracing, route classification and every rate-limit budget. `unmetered` joins between the request
/// middleware and the response security headers, which is the one position that keeps a hydrating
/// page's own bytes and a peer's liveness poll out of every budget while still handing them a header
/// policy. This is also the last position that gets one: anything merged onto the returned router
/// sits outside the response policy, so a route that has to carry it arrives here instead.
pub fn router_with_ui(
    state: Arc<AppState>,
    services: HttpDomainServices,
    ui: MountedRoutes,
    unmetered: Router,
) -> Router {
    let mut route_set = service_routes();
    for registered in state.http_routes() {
        route_set = route_set.merge(registered.routes());
    }
    let (mut router, mut descriptors) = route_set.into_parts();
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
    let (pages, page_descriptors) = ui.into_parts();
    descriptors.extend(page_descriptors);
    let router = router
        .route(
            "/{*path}",
            get(handlers::dispatch_get)
                .put(handlers::dispatch_put)
                .delete(handlers::dispatch_delete)
                .merge(post(handlers::dispatch_post).layer(DefaultBodyLimit::disable())),
        )
        .with_state(Arc::clone(&state))
        .merge(pages)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(request_span)
                .on_response(DefaultOnResponse::new().level(tracing::Level::INFO)),
        );
    let serving = Arc::clone(&state.serving);
    let secured = Arc::clone(&state);
    let classify_routes = serving.rate_limits.enabled() || serving.read_only;
    let router = if serving.rate_limits.enabled() {
        router.layer(middleware::from_fn_with_state(Arc::clone(&state), rate_limit::enforce))
    } else {
        router
    };
    let router = if serving.read_only {
        router.layer(middleware::from_fn_with_state(state, reject_replica_mutation))
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
    let addresses = Arc::clone(&serving);
    let router = router.layer(
        ServiceBuilder::new()
            .layer(MapRequestLayer::new(canonicalize_request_path))
            // Rate limiting and security logs must use one trusted-proxy decision.
            .layer(MapRequestLayer::new(move |request: Request| {
                client_address::attach(&addresses.rate_limits, request)
            }))
            .layer(MapRequestLayer::new(move |request: Request| {
                serving.requests.fetch_add(1, Ordering::Relaxed);
                request
            }))
            .layer(Extension(services)),
    );
    // A merge keeps the right-hand router's fallbacks, so the unmetered routes have to join from the
    // left: merging them in from the right would swap the layered 404 and the layered CONNECT
    // catch-all for the bare ones they carry, dropping those requests out of every middleware.
    crate::response_security::secure_responses(unmetered.merge(router), &secured)
}

/// The four dispatchers, the read-only guard and the rate limiter each read the request path for
/// themselves, so a path spelled two equivalent ways would otherwise split one request into two
/// verdicts. Canonicalizing here, ahead of all of them, leaves them one spelling to agree on.
fn canonicalize_request_path(mut request: Request) -> Request {
    let Cow::Owned(path) = peryx_core::path::canonicalize_path(request.uri().path()) else {
        return request;
    };
    let target = match request.uri().query() {
        Some(query) => format!("{path}?{query}"),
        None => path,
    };
    let mut parts = request.uri().clone().into_parts();
    parts.path_and_query = Some(
        target
            .parse()
            .expect("unescaping unreserved octets leaves a valid target"),
    );
    let uri = Uri::from_parts(parts).expect("only the path and query changed");
    // axum stamps `OriginalUri` while it routes, ahead of every layer, so the handlers reading it
    // would otherwise resolve the spelling the middlewares below have already left behind.
    request.extensions_mut().insert(OriginalUri(uri.clone()));
    *request.uri_mut() = uri;
    request
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
        .route(admin_read(RouteMethod::Get, "/+status"), get(handlers::status))
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
        .route(admin_read(RouteMethod::Get, "/+stats"), get(handlers::stats))
        .route(
            admin_read(RouteMethod::Get, "/+analytics/top-resources"),
            get(handlers::analytics_top),
        )
        .route(
            admin_read(RouteMethod::Get, "/+analytics/unused"),
            get(handlers::analytics_unused),
        )
        .route(
            admin_read(RouteMethod::Get, "/+analytics/groups"),
            get(handlers::analytics_groups),
        )
        .route(
            admin_read(RouteMethod::Get, "/+analytics/sources"),
            get(handlers::analytics_sources),
        )
        .route(
            admin_read(RouteMethod::Get, "/+analytics/timeline"),
            get(handlers::analytics_timeline),
        )
        .route(
            admin_read(RouteMethod::Get, "/+policy/decisions"),
            get(handlers::policy_decisions),
        )
        .route(
            admin_read(RouteMethod::Post, "/+query"),
            post(handlers::pql_query).layer(DefaultBodyLimit::max(16 * 1024)),
        )
        .route(admin_read(RouteMethod::Get, "/+quota"), get(handlers::quota_summary))
        .route(
            admin_read(RouteMethod::Get, "/+quota/repository"),
            get(handlers::quota_repository),
        )
}

fn repository_routes() -> RouteSet {
    RouteSet::new()
        .route(
            admin_read(RouteMethod::Get, "/+repositories"),
            get(handlers::list_repositories),
        )
        .route(
            admin_mutation(RouteMethod::Post, "/+repositories"),
            post(handlers::create_repository).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            admin_read(RouteMethod::Get, "/+repositories/{id}"),
            get(handlers::inspect_repository),
        )
        .route(
            admin_mutation(RouteMethod::Put, "/+repositories/{id}"),
            put(handlers::update_repository).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            admin_mutation(RouteMethod::Post, "/+repositories/{id}/disable"),
            post(handlers::disable_repository),
        )
        .route(
            admin_mutation(RouteMethod::Post, "/+repositories/{id}/enable"),
            post(handlers::enable_repository),
        )
        .route(
            admin_mutation(RouteMethod::Post, "/+cache/purge"),
            post(handlers::purge_cached_resource),
        )
        .route(
            admin_read(RouteMethod::Get, "/+cache"),
            get(handlers::list_cached_contents),
        )
        .route(
            admin_read(RouteMethod::Get, "/+cache/size"),
            get(handlers::size_cached_contents),
        )
        .route(
            admin_read(RouteMethod::Get, "/+cache/fsck"),
            get(handlers::fsck_cached_contents),
        )
        // Both previews only read metadata, so they are POSTs for their request body rather than for
        // any effect, and a replica answers them from the copy it already serves.
        .route(
            admin_read(RouteMethod::Post, "/+retention/plan"),
            post(handlers::retention_plan),
        )
        .route(
            admin_read(RouteMethod::Post, "/+retention/export"),
            post(handlers::retention_export),
        )
        .route(admin_read(RouteMethod::Get, "/+trash"), get(handlers::list_trash))
        .route(
            admin_read(RouteMethod::Get, "/+trash/record"),
            get(handlers::inspect_trash),
        )
}

fn security_routes() -> RouteSet {
    RouteSet::new()
        .route(
            admin_read(RouteMethod::Get, "/+revocations"),
            get(handlers::list_revocations),
        )
        .route(
            admin_read(RouteMethod::Get, "/+revocations/{digest}"),
            get(handlers::inspect_revocation),
        )
        .route(
            admin_mutation(RouteMethod::Put, "/+revocations/{digest}"),
            put(handlers::put_revocation),
        )
        .route(
            admin_mutation(RouteMethod::Post, "/+revocations/{digest}/lift"),
            post(handlers::lift_revocation),
        )
        .route(admin_read(RouteMethod::Get, "/+grants"), get(handlers::list_grants))
        .route(
            admin_mutation(RouteMethod::Post, "/+grants"),
            post(handlers::create_grant),
        )
        .route(
            admin_read(RouteMethod::Get, "/+grants/{id}"),
            get(handlers::inspect_grant),
        )
        .route(
            admin_mutation(RouteMethod::Delete, "/+grants/{id}"),
            delete(handlers::revoke_grant),
        )
        .route(admin_read(RouteMethod::Get, "/+tokens"), get(handlers::list_tokens))
        .route(
            admin_mutation(RouteMethod::Post, "/+tokens"),
            post(handlers::create_token),
        )
        .route(
            admin_read(RouteMethod::Get, "/+tokens/{id}"),
            get(handlers::inspect_token),
        )
        .route(
            admin_mutation(RouteMethod::Delete, "/+tokens/{id}"),
            delete(handlers::revoke_token),
        )
        .route(
            admin_mutation(RouteMethod::Post, "/+tokens/{id}/rotate"),
            post(handlers::rotate_token),
        )
        .route(
            admin_mutation(RouteMethod::Post, "/+jobs/{id}/cancel"),
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

/// A management route that checks a server user's password. The limiter resolves that account before
/// the handler runs, so two administrators behind one address spend separate allowances.
const fn admin_read(method: RouteMethod, path: &'static str) -> RouteDescriptor {
    read(method, path, rate_limit::RouteClass::Admin).authenticating_local_user()
}

const fn admin_mutation(method: RouteMethod, path: &'static str) -> RouteDescriptor {
    mutation(method, path, rate_limit::RouteClass::Admin).authenticating_local_user()
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

        assert_eq!(descriptors.len(), 52);
        assert_eq!(
            descriptors
                .iter()
                .filter(|descriptor| descriptor.posture() == RoutePosture::Mutation)
                .count(),
            14
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
            [2, 2, 44, 4]
        );
    }
}
