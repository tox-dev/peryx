use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use peryx_core::NodeRole;
use peryx_driver::authz::{Decision, DenyReason, ScopedDecision};
use peryx_driver::route_auth::AdminRealm;
use peryx_driver::{AppState, Index, ServingState};
use peryx_events::metrics::UsageInterval;
use peryx_ha::{
    Completeness, CompletenessQuery, CompletenessReport, DayBucket, ExpectedProducer, ProducerId, ProducerReport,
};
use peryx_http::response_security::ProtectedCachePolicy;
use peryx_identity::{Action, Denial, Resource, Scope, UserId, parse_basic};
use serde_json::{Map, Value};

const DEFAULT_LIMIT: usize = 25;
const MAX_LIMIT: usize = 100;
const MAX_REPOSITORY_BYTES: usize = 512;
const SECONDS_PER_DAY: i64 = 86_400;

#[derive(Debug, serde::Deserialize)]
struct Params {
    repository: Option<String>,
    from: Option<i64>,
    to: Option<i64>,
    cursor: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug)]
struct QueryWindow {
    from: Option<i64>,
    to: Option<i64>,
    offset: usize,
    limit: usize,
}

impl QueryWindow {
    fn parse(params: &Params) -> Result<Self, &'static str> {
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
        Ok(Self {
            from: params.from,
            to: params.to,
            offset: params.cursor.as_deref().map_or(Ok(0), decode_cursor)?,
            limit,
        })
    }
}

pub async fn analytics_completeness(State(state): State<Arc<AppState>>, request: Request<Body>) -> Response {
    let (parts, _) = request.into_parts();
    let identity = match authenticate(
        &state.serving,
        &parts.headers,
        parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|authorization| state.recognizes_index_credential(authorization)),
    )
    .await
    {
        Ok(identity) => identity,
        Err(rejection) => return rejection.response(),
    };
    let Ok(Query(params)) = Query::<Params>::try_from_uri(&parts.uri) else {
        return bad_request("invalid analytics query");
    };
    let query = match QueryWindow::parse(&params) {
        Ok(query) => query,
        Err(message) => return bad_request(message),
    };
    let scope = match identity {
        Identity::Local(actor) => authorize_local(&state.serving, params.repository.as_deref(), &actor),
        Identity::EcosystemCredential => params
            .repository
            .as_deref()
            .ok_or(Rejection::Unauthorized)
            .and_then(|route| authorize_ecosystem(&state, &parts.headers, route)),
    };
    let mut response = match scope {
        Ok(scope) => response(&state.serving, &query, scope),
        Err(rejection) => rejection.response(),
    };
    ProtectedCachePolicy::NoStore.apply(response.headers_mut());
    response
}

fn response(state: &ServingState, query: &QueryWindow, scope: CompletenessScope) -> Response {
    let interval = state.metrics.resolve_usage_interval(query.from, query.to);
    let request = CompletenessQuery {
        from_day: interval.from_day,
        to_day: interval.to_day,
        today: state.metrics.current_day(),
        repository: scope.repository,
    };
    let Some(reader) = state.analytics_completeness() else {
        return Rejection::Unavailable.response();
    };
    let Ok(report) = reader.assess(&state.meta, &expected_producers(state), &request) else {
        return Rejection::Unavailable.response();
    };
    page(&report, &interval, query.offset, query.limit, scope.operator)
}

enum Identity {
    Local(UserId),
    EcosystemCredential,
}

async fn authenticate(
    state: &ServingState,
    headers: &HeaderMap,
    ecosystem_credential: bool,
) -> Result<Identity, Rejection> {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or(Rejection::Unauthorized)?;
    if ecosystem_credential {
        return Ok(Identity::EcosystemCredential);
    }
    let credentials = parse_basic(authorization).ok_or(Rejection::Unauthorized)?;
    state
        .users
        .authenticate(&credentials.user, &credentials.password)
        .await
        .map_err(|_| Rejection::Unavailable)?
        .map(Identity::Local)
        .ok_or(Rejection::Unauthorized)
}

struct CompletenessScope {
    repository: Option<String>,
    operator: bool,
}

fn authorize_local(
    state: &ServingState,
    repository: Option<&str>,
    actor: &UserId,
) -> Result<CompletenessScope, Rejection> {
    let Some(route) = repository else {
        require_operator(state, actor)?;
        return Ok(CompletenessScope {
            repository: None,
            operator: true,
        });
    };
    let index = index_by_route(state, route).ok_or(Rejection::NotFound)?;
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

fn authorize_ecosystem(state: &AppState, headers: &HeaderMap, route: &str) -> Result<CompletenessScope, Rejection> {
    let index = index_by_route(&state.serving, route).ok_or(Rejection::Unauthorized)?;
    let authorization = headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok());
    state
        .authorize_index_credential(index, authorization, Action::Read)
        .map_err(|denial| {
            if denial == Denial::Forbidden {
                Rejection::Forbidden
            } else {
                Rejection::Unauthorized
            }
        })?;
    Ok(CompletenessScope {
        repository: Some(index.route.clone()),
        operator: false,
    })
}

fn index_by_route<'state>(state: &'state ServingState, route: &str) -> Option<&'state Index> {
    state.indexes.iter().find(|index| index.route == route)
}

fn require_operator(state: &ServingState, actor: &UserId) -> Result<(), Rejection> {
    require_permission(
        state
            .authorization
            .authorize_scoped(actor, Scope::AnalyticsRead, &Resource::Operator),
    )
}

fn is_operator(state: &ServingState, actor: &UserId) -> bool {
    state
        .authorization
        .authorize_scoped(actor, Scope::AnalyticsRead, &Resource::Operator)
        .decision()
        == Decision::Allow
}

const fn require_permission(decision: ScopedDecision) -> Result<(), Rejection> {
    match decision.decision() {
        Decision::Allow => Ok(()),
        Decision::Deny(DenyReason::NoGrant) => Err(Rejection::NotFound),
        Decision::Deny(DenyReason::StorageUnavailable) => Err(Rejection::Unavailable),
    }
}

fn expected_producers(state: &ServingState) -> Vec<ExpectedProducer> {
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

fn page(
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
    let mut body = Map::from_iter([
        (
            "completeness".to_owned(),
            Value::String(completeness_label(report.completeness).to_owned()),
        ),
        ("interval".to_owned(), interval_json(interval)),
        (
            "totals".to_owned(),
            serde_json::json!({"reads": report.totals.downloads, "bytes": report.totals.bytes}),
        ),
        ("buckets".to_owned(), Value::Array(buckets)),
        ("next_cursor".to_owned(), next_cursor.map_or(Value::Null, Value::String)),
    ]);
    if operator {
        body.extend([
            ("frontier_day".to_owned(), day_json(report.frontier_day)),
            ("required_day".to_owned(), day_json(report.required_day)),
            ("lag_days".to_owned(), day_json(report.lag_days)),
            (
                "producers".to_owned(),
                Value::Array(report.producers.iter().map(producer_json).collect()),
            ),
        ]);
    }
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

const fn completeness_label(value: Completeness) -> &'static str {
    match value {
        Completeness::Complete => "complete",
        Completeness::Delayed => "delayed",
        Completeness::Unavailable => "unavailable",
    }
}

fn bucket_json(bucket: &DayBucket) -> Value {
    serde_json::json!({
        "day": bucket.day,
        "start_unix": bucket.day * SECONDS_PER_DAY,
        "end_unix": (bucket.day + 1) * SECONDS_PER_DAY,
        "reads": bucket.downloads,
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

fn day_json(day: Option<i64>) -> Value {
    day.map_or(Value::Null, |value| serde_json::json!(value))
}

fn encode_cursor(offset: usize) -> String {
    URL_SAFE_NO_PAD.encode(offset.to_string())
}

fn decode_cursor(cursor: &str) -> Result<usize, &'static str> {
    URL_SAFE_NO_PAD
        .decode(cursor)
        .ok()
        .and_then(|bytes| std::str::from_utf8(&bytes).ok()?.parse().ok())
        .ok_or("invalid analytics cursor")
}

#[derive(Clone, Copy)]
enum Rejection {
    Forbidden,
    NotFound,
    Unavailable,
    Unauthorized,
}

impl Rejection {
    fn response(self) -> Response {
        let mut response = match self {
            Self::Forbidden => StatusCode::FORBIDDEN.into_response(),
            Self::NotFound => StatusCode::NOT_FOUND.into_response(),
            Self::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({"error": "analytics service unavailable"})),
            )
                .into_response(),
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, AdminRealm::Analytics.challenge())],
            )
                .into_response(),
        };
        ProtectedCachePolicy::NoStore.apply(response.headers_mut());
        response
    }
}

fn bad_request(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(serde_json::json!({"error": message})),
    )
        .into_response()
}
