//! The `/+analytics/*` usage query family: authorized top, unused, version, source, and time-bucket
//! views over the retained daily aggregate.
//!
//! Every view authenticates, authorizes a repository or operator scope before it aggregates, resolves
//! an explicit day window, and paginates with an opaque offset cursor. The source dimension is
//! operator-only, so a repository-scoped caller cannot inspect where a cache miss routed to.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Request, StatusCode, Uri, header};
use axum::response::{IntoResponse as _, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Map, Value};

use peryx_core::NodeRole;
use peryx_driver::authz::{Decision, DenyReason, ScopedDecision};
use peryx_driver::state::AppState;
use peryx_events::metrics::UsageInterval;
use peryx_ha::{
    Completeness, CompletenessQuery, CompletenessReport, DayBucket, ExpectedProducer, ProducerId, ProducerReport,
};
use peryx_identity::{Action, Resource, Scope, UserId, parse_basic};

use crate::response_security::ProtectedCachePolicy;

const DEFAULT_LIMIT: usize = 25;
const MAX_LIMIT: usize = 100;
const MAX_REPOSITORY_BYTES: usize = 512;
const SECONDS_PER_DAY: i64 = 86_400;

/// The five usage views, each keying its rows and choosing whether it needs operator authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UsageView {
    Top,
    Unused,
    Versions,
    Sources,
    Timeline,
}

impl UsageView {
    const fn rows_key(self) -> &'static str {
        match self {
            Self::Top => "packages",
            Self::Unused => "unused",
            Self::Versions => "versions",
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

pub async fn analytics_versions(state: State<Arc<AppState>>, request: Request<Body>) -> Response {
    respond(state, request, UsageView::Versions).await
}

pub async fn analytics_sources(state: State<Arc<AppState>>, request: Request<Body>) -> Response {
    respond(state, request, UsageView::Sources).await
}

pub async fn analytics_timeline(state: State<Arc<AppState>>, request: Request<Body>) -> Response {
    respond(state, request, UsageView::Timeline).await
}

async fn respond(State(state): State<Arc<AppState>>, request: Request<Body>, view: UsageView) -> Response {
    let (parts, _) = request.into_parts();
    let mut response = usage_response(&state, &parts.headers, &parts.uri, view).await;
    ProtectedCachePolicy::NoStore.apply(response.headers_mut());
    response
}

pub async fn analytics_completeness(State(state): State<Arc<AppState>>, request: Request<Body>) -> Response {
    let (parts, _) = request.into_parts();
    let mut response = completeness_response(&state, &parts.headers, &parts.uri).await;
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

async fn usage_response(state: &AppState, headers: &HeaderMap, uri: &Uri, view: UsageView) -> Response {
    let identity = match authenticate(state, headers).await {
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
    let interval = state.metrics.resolve_usage_interval(request.from, request.to);
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

/// Run the view's aggregation into serialized rows, already deterministically ordered.
fn collect(state: &AppState, view: UsageView, repository: Option<&str>, interval: &UsageInterval) -> Vec<Value> {
    let metrics = &state.metrics;
    match view {
        UsageView::Top => rows(metrics.usage_top(repository, interval)),
        UsageView::Unused => rows(metrics.usage_unused(repository, interval)),
        UsageView::Versions => rows(metrics.usage_versions(repository, interval)),
        UsageView::Sources => rows(metrics.usage_sources(repository, interval)),
        UsageView::Timeline => rows(metrics.usage_timeline(repository, interval)),
    }
}

fn rows<T: serde::Serialize>(rows: Vec<T>) -> Vec<Value> {
    rows.into_iter()
        .map(|row| serde_json::to_value(row).expect("usage row serializes"))
        .collect()
}

/// Slice the deterministic row set at the cursor offset and build the view's response envelope.
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
    LegacyToken,
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

impl From<super::LegacyDenied> for Rejection {
    fn from(denied: super::LegacyDenied) -> Self {
        match denied {
            super::LegacyDenied::Forbidden => Self::Forbidden,
            super::LegacyDenied::Unauthorized => Self::Unauthorized,
        }
    }
}

async fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<Identity, Rejection> {
    let credentials = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_basic)
        .ok_or(Rejection::Unauthorized)?;
    if credentials.user == "__token__" {
        return Ok(Identity::LegacyToken);
    }
    state
        .users
        .authenticate(&credentials.user, &credentials.password)
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
        Identity::LegacyToken => authorize_legacy(state, headers, repository),
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
    let decision =
        state
            .authorization
            .authorize_scoped(actor, Scope::RepositoryRead, &Resource::Repository(index.name.clone()));
    require_permission(decision)?;
    Ok(UsageScope {
        repository: Some(index.route.clone()),
    })
}

fn authorize_legacy(state: &AppState, headers: &HeaderMap, repository: Option<&str>) -> Result<UsageScope, Rejection> {
    let route = repository.ok_or(Rejection::Unauthorized)?;
    let index = super::authorize_legacy_route(state, headers, route, Action::Read)?;
    Ok(UsageScope {
        repository: Some(index.route.clone()),
    })
}

fn require_operator(state: &AppState, actor: &UserId) -> Result<(), Rejection> {
    require_permission(
        state
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
        [(header::WWW_AUTHENTICATE, "Basic realm=\"peryx-analytics\"")],
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

/// The authorized reach of a completeness query: the repository the totals are confined to (or `None`
/// for a cross-repository query), and whether the caller holds operator authority, which gates the
/// per-datacenter producer frontier and the cluster lag from a repository-only reader.
#[derive(Debug)]
struct CompletenessScope {
    repository: Option<String>,
    operator: bool,
}

async fn completeness_response(state: &AppState, headers: &HeaderMap, uri: &Uri) -> Response {
    let identity = match authenticate(state, headers).await {
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
    let scope = match authorize_completeness(state, headers, params.repository.as_deref(), &identity) {
        Ok(scope) => scope,
        Err(rejection) => return rejection.response(),
    };
    let interval = state.metrics.resolve_usage_interval(request.from, request.to);
    let query = CompletenessQuery {
        from_day: interval.from_day,
        to_day: interval.to_day,
        today: state.metrics.current_day(),
        repository: scope.repository.clone(),
    };
    let Some(reader) = state.analytics_completeness() else {
        return Rejection::Unavailable.response();
    };
    let Ok(report) = reader.assess(&state.meta, &expected_producers(state), &query) else {
        return Rejection::Unavailable.response();
    };
    completeness_page(&report, &interval, request.offset, request.limit, scope.operator)
}

/// Authorize a completeness query. An operator-wide query (no repository) needs operator authority; a
/// repository query is admitted for an operator or for a reader of that repository, and only the
/// operator sees the producer frontier. A repository upload token reads its own repository's totals but
/// never the producer frontier.
fn authorize_completeness(
    state: &AppState,
    headers: &HeaderMap,
    repository: Option<&str>,
    identity: &Identity,
) -> Result<CompletenessScope, Rejection> {
    let actor = match identity {
        Identity::Local(actor) => actor,
        Identity::LegacyToken => {
            let route = repository.ok_or(Rejection::Unauthorized)?;
            let index = super::authorize_legacy_route(state, headers, route, Action::Read)?;
            return Ok(CompletenessScope {
                repository: Some(index.route.clone()),
                operator: false,
            });
        }
    };
    let Some(route) = repository else {
        require_operator(state, actor)?;
        return Ok(CompletenessScope {
            repository: None,
            operator: true,
        });
    };
    let index = super::index_by_route(state, route).ok_or(Rejection::NotFound)?;
    if is_operator(state, actor) {
        return Ok(CompletenessScope {
            repository: Some(index.route.clone()),
            operator: true,
        });
    }
    require_permission(state.authorization.authorize_scoped(
        actor,
        Scope::RepositoryRead,
        &Resource::Repository(index.name.clone()),
    ))?;
    Ok(CompletenessScope {
        repository: Some(index.route.clone()),
        operator: false,
    })
}

fn is_operator(state: &AppState, actor: &UserId) -> bool {
    matches!(
        state
            .authorization
            .authorize_scoped(actor, Scope::AnalyticsRead, &Resource::Operator)
            .decision(),
        Decision::Allow
    )
}

/// The producers a completeness query expects: every writer member of the configured topology, keyed by
/// its stable node identity and labelled with its datacenter. A configured writer that has delivered no
/// batch is still expected, so a silent datacenter marks the range incomplete rather than vanishing.
fn expected_producers(state: &AppState) -> Vec<ExpectedProducer> {
    state
        .availability_topology()
        .members
        .iter()
        .filter(|member| member.role == NodeRole::Writer)
        .map(|member| ExpectedProducer {
            producer: ProducerId(member.node.clone()),
            dc: member.dc.clone(),
        })
        .collect()
}

/// Slice the day buckets at the cursor offset and build the response envelope. The verdict, interval,
/// totals, and buckets are always present; the frontier, lag, and per-producer coverage are added only
/// for an operator, so a repository reader never learns which datacenters exist or how they lag.
fn completeness_page(
    report: &CompletenessReport,
    interval: &UsageInterval,
    offset: usize,
    limit: usize,
    operator: bool,
) -> Response {
    let mut buckets: Vec<Value> = report.buckets.iter().map(bucket_json).collect();
    buckets.drain(0..offset.min(buckets.len()));
    let next_cursor = (buckets.len() > limit).then(|| encode_cursor(offset + limit));
    buckets.truncate(limit);
    let mut body = Map::new();
    body.insert(
        "completeness".to_owned(),
        Value::String(completeness_label(report.completeness).to_owned()),
    );
    body.insert("interval".to_owned(), interval_json(interval));
    body.insert(
        "totals".to_owned(),
        serde_json::json!({"downloads": report.totals.downloads, "bytes": report.totals.bytes}),
    );
    body.insert("buckets".to_owned(), Value::Array(buckets));
    body.insert("next_cursor".to_owned(), next_cursor.map_or(Value::Null, Value::String));
    if operator {
        body.insert("frontier_day".to_owned(), day_json(report.frontier_day));
        body.insert("required_day".to_owned(), day_json(report.required_day));
        body.insert("lag_days".to_owned(), day_json(report.lag_days));
        body.insert(
            "producers".to_owned(),
            Value::Array(report.producers.iter().map(producer_json).collect()),
        );
    }
    axum::Json(Value::Object(body)).into_response()
}

const fn completeness_label(completeness: Completeness) -> &'static str {
    match completeness {
        Completeness::Complete => "complete",
        Completeness::Delayed => "delayed",
        Completeness::Unavailable => "unavailable",
    }
}

fn day_json(day: Option<i64>) -> Value {
    day.map_or(Value::Null, |day| serde_json::json!(day))
}

fn bucket_json(bucket: &DayBucket) -> Value {
    serde_json::json!({
        "day": bucket.day,
        "start_unix": bucket.day * SECONDS_PER_DAY,
        "end_unix": (bucket.day + 1) * SECONDS_PER_DAY,
        "downloads": bucket.downloads,
        "bytes": bucket.bytes,
    })
}

fn producer_json(producer: &ProducerReport) -> Value {
    serde_json::json!({
        "producer": producer.producer.0,
        "dc": producer.dc,
        "state": completeness_label(producer.state),
        "accepted_epoch": producer.accepted.map(|(epoch, _)| epoch.0),
        "accepted_day": producer.accepted.map(|(_, day)| day),
    })
}
