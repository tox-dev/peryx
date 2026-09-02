//! Every view authenticates, authorizes a repository or operator scope before it aggregates, resolves
//! an explicit day window, and paginates with an opaque offset cursor. The source dimension is
//! operator-only, so a repository-scoped caller cannot inspect where a cache miss routed to.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{Extensions, HeaderMap, Request, StatusCode, Uri, header};
use axum::response::{IntoResponse as _, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Map, Value};

use peryx_driver::authz::{Decision, DenyReason, ScopedDecision};
use peryx_driver::state::AppState;
use peryx_events::metrics::UsageInterval;
use peryx_identity::{Action, Resource, Scope, UserId, parse_basic};

use crate::response_security::ProtectedCachePolicy;
use peryx_driver::route_auth::AdminRealm;

const DEFAULT_LIMIT: usize = 25;
const MAX_LIMIT: usize = 100;
const MAX_REPOSITORY_BYTES: usize = 512;
const SECONDS_PER_DAY: i64 = 86_400;

/// The five usage views, each keying its rows and choosing whether it needs operator authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UsageView {
    Top,
    Unused,
    Groups,
    Sources,
    Timeline,
}

impl UsageView {
    const fn rows_key(self) -> &'static str {
        match self {
            Self::Top => "resources",
            Self::Unused => "unused",
            Self::Groups => "groups",
            Self::Sources => "sources",
            Self::Timeline => "buckets",
        }
    }

    /// The source dimension is operator-scoped, so its view refuses a repository-only caller.
    const fn requires_operator(self) -> bool {
        matches!(self, Self::Sources)
    }
}

pub async fn analytics_top(state: State<Arc<AppState>>, request: Request<Body>) -> Response {
    respond(state, request, UsageView::Top).await
}

pub async fn analytics_unused(state: State<Arc<AppState>>, request: Request<Body>) -> Response {
    respond(state, request, UsageView::Unused).await
}

pub async fn analytics_groups(state: State<Arc<AppState>>, request: Request<Body>) -> Response {
    respond(state, request, UsageView::Groups).await
}

pub async fn analytics_sources(state: State<Arc<AppState>>, request: Request<Body>) -> Response {
    respond(state, request, UsageView::Sources).await
}

pub async fn analytics_timeline(state: State<Arc<AppState>>, request: Request<Body>) -> Response {
    respond(state, request, UsageView::Timeline).await
}

async fn respond(State(state): State<Arc<AppState>>, request: Request<Body>, view: UsageView) -> Response {
    let (parts, _) = request.into_parts();
    let mut response = usage_response(&state, &parts.headers, &parts.extensions, &parts.uri, view).await;
    ProtectedCachePolicy::NoStore.apply(response.headers_mut());
    response
}

/// The `/+analytics/*` query selectors shared by every view.
#[derive(Debug, serde::Deserialize)]
struct UsageParams {
    repository: Option<String>,
    from: Option<i64>,
    to: Option<i64>,
    cursor: Option<String>,
    limit: Option<usize>,
}

async fn usage_response(
    state: &AppState,
    headers: &HeaderMap,
    extensions: &Extensions,
    uri: &Uri,
    view: UsageView,
) -> Response {
    let identity = match authenticate(state, headers, extensions).await {
        Ok(identity) => identity,
        Err(rejection) => return rejection.response(),
    };
    let Ok(Query(params)) = Query::<UsageParams>::try_from_uri(uri) else {
        return bad_request("invalid analytics query");
    };
    let request = match UsageRequest::parse(&params) {
        Ok(request) => request,
        Err(message) => return bad_request(message),
    };
    let scope = match authorize(state, headers, view, params.repository.as_deref(), &identity) {
        Ok(scope) => scope,
        Err(rejection) => return rejection.response(),
    };
    let interval = state.serving.metrics.resolve_usage_interval(request.from, request.to);
    let rows = collect(state, view, scope.repository.as_deref(), &interval);
    page(view, rows, request.offset, request.limit, &interval)
}

/// A validated query: the time bounds still in Unix seconds, plus the resolved page offset and limit.
#[derive(Debug)]
struct UsageRequest {
    from: Option<i64>,
    to: Option<i64>,
    offset: usize,
    limit: usize,
}

impl UsageRequest {
    fn parse(params: &UsageParams) -> Result<Self, &'static str> {
        let limit = params.limit.unwrap_or(DEFAULT_LIMIT);
        if !(1..=MAX_LIMIT).contains(&limit) {
            return Err("limit must be between 1 and 100");
        }
        if params
            .repository
            .as_deref()
            .is_some_and(|value| value.len() > MAX_REPOSITORY_BYTES)
        {
            return Err("repository filter exceeds 512 bytes");
        }
        if let (Some(from), Some(to)) = (params.from, params.to)
            && from > to
        {
            return Err("time range start is after its end");
        }
        let offset = match params.cursor.as_deref() {
            Some(cursor) => decode_cursor(cursor).ok_or("invalid analytics cursor")?,
            None => 0,
        };
        Ok(Self {
            from: params.from,
            to: params.to,
            offset,
            limit,
        })
    }
}

fn collect(state: &AppState, view: UsageView, repository: Option<&str>, interval: &UsageInterval) -> Vec<Value> {
    let metrics = &state.serving.metrics;
    match view {
        UsageView::Top => rows(metrics.usage_top(repository, interval)),
        UsageView::Unused => rows(metrics.usage_unused(repository, interval)),
        UsageView::Groups => rows(metrics.usage_groups(repository, interval)),
        UsageView::Sources => rows(metrics.usage_sources(repository, interval)),
        UsageView::Timeline => rows(metrics.usage_timeline(repository, interval)),
    }
}

fn rows<T: serde::Serialize>(rows: Vec<T>) -> Vec<Value> {
    rows.into_iter()
        .map(|row| serde_json::to_value(row).expect("usage row serializes"))
        .collect()
}

fn page(view: UsageView, mut rows: Vec<Value>, offset: usize, limit: usize, interval: &UsageInterval) -> Response {
    rows.drain(0..offset.min(rows.len()));
    let next_cursor = (rows.len() > limit).then(|| encode_cursor(offset + limit));
    rows.truncate(limit);
    let mut body = Map::new();
    body.insert(view.rows_key().to_owned(), Value::Array(rows));
    body.insert("interval".to_owned(), interval_json(interval));
    body.insert("next_cursor".to_owned(), next_cursor.map_or(Value::Null, Value::String));
    axum::Json(Value::Object(body)).into_response()
}

fn interval_json(interval: &UsageInterval) -> Value {
    serde_json::json!({
        "from_day": interval.from_day,
        "to_day": interval.to_day,
        "from_unix": interval.from_day * SECONDS_PER_DAY,
        "to_unix": (interval.to_day + 1) * SECONDS_PER_DAY,
        "retained_from_day": interval.retained_from_day,
        "window_clamped_to_retention": interval.window_clamped_to_retention,
    })
}

fn encode_cursor(offset: usize) -> String {
    URL_SAFE_NO_PAD.encode(offset.to_string())
}

fn decode_cursor(cursor: &str) -> Option<usize> {
    let bytes = URL_SAFE_NO_PAD.decode(cursor).ok()?;
    std::str::from_utf8(&bytes).ok()?.parse().ok()
}

#[derive(Debug)]
enum Identity {
    Local(UserId),
    EcosystemCredential,
}

#[derive(Debug)]
struct UsageScope {
    /// The index route the aggregation is confined to, or `None` for an operator-wide query.
    repository: Option<String>,
}

#[derive(Debug, Clone, Copy)]
enum Rejection {
    Forbidden,
    NotFound,
    Unavailable,
    Unauthorized,
}

impl Rejection {
    fn response(self) -> Response {
        match self {
            Self::Forbidden => StatusCode::FORBIDDEN.into_response(),
            Self::NotFound => StatusCode::NOT_FOUND.into_response(),
            Self::Unavailable => unavailable(),
            Self::Unauthorized => unauthorized(),
        }
    }
}

impl From<super::EcosystemCredentialDenied> for Rejection {
    fn from(denied: super::EcosystemCredentialDenied) -> Self {
        match denied {
            super::EcosystemCredentialDenied::Forbidden => Self::Forbidden,
            super::EcosystemCredentialDenied::Unauthorized => Self::Unauthorized,
        }
    }
}

async fn authenticate(state: &AppState, headers: &HeaderMap, extensions: &Extensions) -> Result<Identity, Rejection> {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or(Rejection::Unauthorized)?;
    if state.recognizes_index_credential(authorization) {
        return Ok(Identity::EcosystemCredential);
    }
    let credentials = parse_basic(authorization).ok_or(Rejection::Unauthorized)?;
    state
        .serving
        .users
        .authenticate_request(extensions, &credentials)
        .await
        .map_err(|_| Rejection::Unavailable)?
        .map(Identity::Local)
        .ok_or(Rejection::Unauthorized)
}

fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    view: UsageView,
    repository: Option<&str>,
    identity: &Identity,
) -> Result<UsageScope, Rejection> {
    if view.requires_operator() {
        return authorize_source_view(state, repository, identity);
    }
    match identity {
        Identity::Local(actor) => authorize_local(state, actor, repository),
        Identity::EcosystemCredential => authorize_ecosystem(state, headers, repository),
    }
}

/// The source view needs operator authority; a repository-only credential cannot reach it. A named
/// repository narrows the operator query but must still exist.
fn authorize_source_view(
    state: &AppState,
    repository: Option<&str>,
    identity: &Identity,
) -> Result<UsageScope, Rejection> {
    let Identity::Local(actor) = identity else {
        return Err(Rejection::Forbidden);
    };
    require_operator(state, actor)?;
    let repository = match repository {
        Some(route) => Some(
            super::index_by_route(state, route)
                .ok_or(Rejection::NotFound)?
                .route
                .clone(),
        ),
        None => None,
    };
    Ok(UsageScope { repository })
}

fn authorize_local(state: &AppState, actor: &UserId, repository: Option<&str>) -> Result<UsageScope, Rejection> {
    let Some(route) = repository else {
        require_operator(state, actor)?;
        return Ok(UsageScope { repository: None });
    };
    let index = super::index_by_route(state, route).ok_or(Rejection::NotFound)?;
    let decision = state.serving.authorization.authorize_scoped(
        actor,
        Scope::RepositoryRead,
        &Resource::Repository(index.name.clone()),
    );
    require_permission(decision)?;
    Ok(UsageScope {
        repository: Some(index.route.clone()),
    })
}

fn authorize_ecosystem(
    state: &AppState,
    headers: &HeaderMap,
    repository: Option<&str>,
) -> Result<UsageScope, Rejection> {
    let route = repository.ok_or(Rejection::Unauthorized)?;
    let index = super::authorize_ecosystem_credential(state, headers, route, Action::Read)?;
    Ok(UsageScope {
        repository: Some(index.route.clone()),
    })
}

fn require_operator(state: &AppState, actor: &UserId) -> Result<(), Rejection> {
    require_permission(
        state
            .serving
            .authorization
            .authorize_scoped(actor, Scope::AnalyticsRead, &Resource::Operator),
    )
}

const fn require_permission(decision: ScopedDecision) -> Result<(), Rejection> {
    match decision.decision() {
        Decision::Allow => Ok(()),
        Decision::Deny(DenyReason::NoGrant) => Err(Rejection::NotFound),
        Decision::Deny(DenyReason::StorageUnavailable) => Err(Rejection::Unavailable),
    }
}

fn bad_request(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(serde_json::json!({ "error": message })),
    )
        .into_response()
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, AdminRealm::Analytics.challenge())],
    )
        .into_response()
}

fn unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(serde_json::json!({"error": "analytics service unavailable"})),
    )
        .into_response()
}
