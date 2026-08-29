//! Preview and export share the CLI query and never mutate metadata. Cursors bind policy version,
//! metadata frontier, and repository so stale or mismatched resumes fail before streaming.

use std::sync::Arc;

use axum::Extension;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use peryx_driver::authz::Decision;
use peryx_driver::http_services::HttpDomainServices;
use peryx_driver::retention::{RetentionExport, RetentionPage, RetentionPlanError, RetentionQuery, decode_cursor};
use peryx_driver::serving::RetentionDriver;
use peryx_driver::state::AppState;
use peryx_identity::{Resource, Scope, UserId, parse_basic};
use peryx_policy::{RetentionConfig, RetentionDecision, RetentionPolicy, RetentionSelector, RetentionSummary};

use crate::response_security::ProtectedCachePolicy;

const MAX_BODY_BYTES: usize = 64 * 1024;
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 1000;
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanRequest {
    repository: String,
    #[serde(default)]
    keep: Vec<RetentionSelector>,
    #[serde(default)]
    expire: Vec<RetentionSelector>,
    /// Resume token from a prior page or export, carrying the offset and the identity to match.
    #[serde(default)]
    cursor: Option<String>,
    /// Page size for `plan`, from 1 through 1000; ignored by `export`, which streams the whole plan.
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(serde::Serialize)]
struct PlanResponse {
    summary: RetentionSummary,
    candidates: Vec<RetentionDecision>,
    next_cursor: Option<String>,
}

/// # Panics
/// Panics if request resolution returns a driver without retention support.
pub async fn retention_plan(
    State(state): State<Arc<AppState>>,
    Extension(services): Extension<HttpDomainServices>,
    request: Request<Body>,
) -> Response {
    let request = match Prepared::from_request(&state, request).await {
        Ok(prepared) => prepared,
        Err(response) => return response,
    };
    let Some(_permit) = services.retention().try_enter(&request.repository) else {
        return busy();
    };
    let after = request.after;
    let mut candidates = Vec::new();
    let query = RetentionQuery {
        index: &request.repository,
        ecosystem: &request.ecosystem,
        policy: &request.policy,
        now: request.evaluated_at,
        after,
        limit: Some(request.limit),
        expect: request.expect,
    };
    match services
        .retention()
        .plan(request.driver.as_ref(), &query, &mut |_| Ok(()), &mut |decision| {
            candidates.push(decision.clone());
            Ok(())
        }) {
        Ok(RetentionPage {
            summary, next_cursor, ..
        }) => {
            let body = PlanResponse {
                summary,
                candidates,
                next_cursor,
            };
            let mut response = axum::Json(body).into_response();
            ProtectedCachePolicy::NoStore.apply(response.headers_mut());
            response
        }
        Err(error) => plan_error(&error),
    }
}

pub async fn retention_export(
    State(state): State<Arc<AppState>>,
    Extension(services): Extension<HttpDomainServices>,
    request: Request<Body>,
) -> Response {
    let request = match Prepared::from_request(&state, request).await {
        Ok(prepared) => prepared,
        Err(response) => return response,
    };
    let Some(permit) = services.retention().try_enter(&request.repository) else {
        return busy();
    };
    let export = RetentionExport {
        index: request.repository,
        ecosystem: request.ecosystem,
        policy: request.policy,
        now: request.evaluated_at,
        after: request.after,
        expect: request.expect,
    };
    let (summary, body) = match services.retention().export(request.driver, export, permit).await {
        Ok(started) => started,
        Err(error) => return plan_error(&error),
    };
    let mut response = (StatusCode::OK, body).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/x-ndjson"));
    headers.insert(header::ETAG, plan_etag(summary));
    // The stream is unique to one snapshot, so a byte range cannot resume it; a client restarts from
    // the last decision it consumed by presenting that page's cursor back.
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("none"));
    ProtectedCachePolicy::NoStore.apply(headers);
    response
}

/// A request that authenticated, resolved to a repository with a retention-planning driver, and parsed
/// its rules and resume position.
struct Prepared {
    driver: Arc<dyn RetentionDriver>,
    repository: String,
    ecosystem: String,
    policy: RetentionPolicy,
    evaluated_at: Option<i64>,
    after: u64,
    expect: Option<RetentionSummary>,
    limit: usize,
}

impl Prepared {
    async fn from_request(state: &AppState, request: Request<Body>) -> Result<Self, Response> {
        let (parts, body) = request.into_parts();
        administrator(state, &parts.headers).await?;
        if !super::is_json(&parts.headers) {
            return Err(problem(StatusCode::UNSUPPORTED_MEDIA_TYPE, "request body must be JSON"));
        }
        let Ok(body) = axum::body::to_bytes(body, MAX_BODY_BYTES).await else {
            return Err(problem(StatusCode::PAYLOAD_TOO_LARGE, "request body is too large"));
        };
        let request: PlanRequest = serde_json::from_slice(&body)
            .map_err(|_| problem(StatusCode::UNPROCESSABLE_ENTITY, "invalid request body"))?;
        let limit = match request.limit {
            Some(limit) if limit == 0 || limit > MAX_LIMIT => {
                return Err(problem(StatusCode::BAD_REQUEST, "limit must be between 1 and 1000"));
            }
            Some(limit) => limit,
            None => DEFAULT_LIMIT,
        };
        // A repository the caller cannot resolve is a 404, not a distinct error, so an administrator
        // cannot probe which routes exist by the shape of the failure.
        let index = super::index_by_route(state, &request.repository).ok_or_else(not_found)?;
        let driver = state
            .driver_set()
            .get_retention(&index.ecosystem)
            .ok_or_else(not_found)?
            .clone();
        let (after, expect, evaluated_at) = match &request.cursor {
            Some(cursor) => {
                let resume = decode_cursor(cursor).map_err(|reason| problem(StatusCode::BAD_REQUEST, &reason))?;
                if resume.repository != index.name || resume.ecosystem != index.ecosystem.as_str() {
                    return Err(stale());
                }
                (resume.after, Some(resume.expect), resume.evaluated_at)
            }
            None => (0, None, Some((state.serving.clock)())),
        };
        let policy = RetentionPolicy::compile(
            &RetentionConfig {
                keep: request.keep,
                expire: request.expire,
            },
            |name| {
                state
                    .driver_set()
                    .get_name(&index.ecosystem)
                    .map_or_else(|| name.to_owned(), |driver| driver.normalize_name(name))
            },
        );
        driver
            .validate_retention(&policy)
            .map_err(|reason| problem(StatusCode::UNPROCESSABLE_ENTITY, &reason))?;
        Ok(Self {
            driver,
            repository: index.name.clone(),
            ecosystem: index.ecosystem.as_str().to_owned(),
            policy,
            evaluated_at,
            after,
            expect,
            limit,
        })
    }
}

async fn administrator(state: &AppState, headers: &HeaderMap) -> Result<UserId, Response> {
    let credentials = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_basic)
        .ok_or_else(unauthorized)?;
    let actor = state
        .serving
        .users
        .authenticate(&credentials.user, &credentials.password)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(unauthorized)?;
    let decision = state
        .serving
        .authorization
        .authorize_scoped(&actor, Scope::AdministrationRead, &Resource::Operator);
    if decision.decision() != Decision::Allow {
        return Err(not_found());
    }
    Ok(actor)
}

fn plan_etag(summary: RetentionSummary) -> HeaderValue {
    let frontier = summary.frontier;
    HeaderValue::from_str(&format!(
        "\"{}-{}-{}-{}\"",
        summary.policy_version, frontier.repository, frontier.catalog, frontier.policy
    ))
    .expect("a plan etag is ascii")
}

fn plan_error(error: &RetentionPlanError) -> Response {
    match error {
        RetentionPlanError::Stale { .. } => stale(),
        // A buffered page never interrupts its own sink, and an export interruption means the client
        // already left; both remaining cases are a failed read the caller cannot act on.
        RetentionPlanError::Interrupted(_) | RetentionPlanError::Store(_) => {
            problem(StatusCode::INTERNAL_SERVER_ERROR, "retention plan failed")
        }
    }
}

fn stale() -> Response {
    problem(StatusCode::CONFLICT, "the plan cursor is stale: the repository changed")
}

fn busy() -> Response {
    problem(
        StatusCode::TOO_MANY_REQUESTS,
        "too many concurrent retention plans for this repository",
    )
}

fn not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"peryx-administration\"")],
    )
        .into_response()
}

fn unavailable() -> Response {
    problem(StatusCode::SERVICE_UNAVAILABLE, "retention service unavailable")
}

fn problem(status: StatusCode, message: &str) -> Response {
    (status, axum::Json(serde_json::json!({"error": message}))).into_response()
}
