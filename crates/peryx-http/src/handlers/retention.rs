//! Retention-plan preview and export over the administrator HTTP API.
//!
//! `POST /+retention/plan` returns one bounded page of ordered removal candidates; `POST
//! /+retention/export` streams the whole plan as [JSON Lines](https://jsonlines.org/). Both name a
//! repository and the retention rules to evaluate, and both reuse the shared
//! [query](peryx_driver::retention) so the API and the CLI produce the same ordered candidates. Neither
//! mutates metadata: a preview only reads.
//!
//! Every response carries the plan identity (policy version and metadata frontier). A page's cursor and
//! an export's first line fold it back in, so a later apply can reject stale input and a resumed export
//! that no longer matches its repository is refused before it streams.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use peryx_driver::authz::Decision;
use peryx_driver::retention::{
    RetentionExport, RetentionPage, RetentionPlanError, RetentionQuery, decode_cursor, export_body, plan, summary,
};
use peryx_driver::serving::EcosystemDriver;
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

/// One page of ordered removal candidates for a repository.
///
/// # Panics
/// Panics if request resolution returns a driver without retention support.
pub async fn retention_plan(State(state): State<Arc<AppState>>, request: Request<Body>) -> Response {
    let request = match Prepared::from_request(&state, request).await {
        Ok(prepared) => prepared,
        Err(response) => return response,
    };
    let Some(_permit) = state.retention_gates.try_enter(&request.repository) else {
        return busy();
    };
    let after = request.after;
    let mut candidates = Vec::new();
    let query = RetentionQuery {
        index: &request.repository,
        policy: &request.policy,
        now: Some((state.clock)()),
        after,
        limit: Some(request.limit),
        expect: request.expect,
    };
    let driver = request
        .driver
        .capabilities()
        .retention
        .expect("request resolution requires retention support");
    match plan(driver, &state.meta, &query, &mut |decision| {
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

/// The whole plan streamed as JSON Lines, resumable from its documented boundary.
pub async fn retention_export(State(state): State<Arc<AppState>>, request: Request<Body>) -> Response {
    let request = match Prepared::from_request(&state, request).await {
        Ok(prepared) => prepared,
        Err(response) => return response,
    };
    let summary = match summary(&state.meta, &request.repository, &request.policy) {
        Ok(summary) => summary,
        Err(reason) => return plan_error(&RetentionPlanError::Store(reason)),
    };
    // Reject a resume whose repository has shifted before any row streams, so a saved export never
    // splices two snapshots.
    if request.expect.is_some_and(|expected| expected != summary) {
        return stale();
    }
    let Some(permit) = state.retention_gates.try_enter(&request.repository) else {
        return busy();
    };
    let export = RetentionExport {
        index: request.repository,
        policy: request.policy,
        now: Some((state.clock)()),
        after: request.after,
        summary,
    };
    let body = export_body(request.driver, state.meta.clone(), export, permit);
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
    driver: Arc<dyn EcosystemDriver>,
    repository: String,
    policy: RetentionPolicy,
    after: u64,
    expect: Option<RetentionSummary>,
    limit: usize,
}

impl Prepared {
    async fn from_request(state: &AppState, request: Request<Body>) -> Result<Self, Response> {
        let (parts, body) = request.into_parts();
        administrator(state, &parts.headers).await?;
        if !is_json(&parts.headers) {
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
        let driver = state.driver_for(index.ecosystem).ok_or_else(not_found)?.clone();
        if driver.capabilities().retention.is_none() {
            return Err(not_found());
        }
        let (after, expect) = match &request.cursor {
            Some(cursor) => {
                let resume = decode_cursor(cursor).map_err(|reason| problem(StatusCode::BAD_REQUEST, &reason))?;
                (resume.after, Some(resume.expect))
            }
            None => (0, None),
        };
        let policy = RetentionPolicy::compile(&RetentionConfig {
            keep: request.keep,
            expire: request.expire,
        });
        Ok(Self {
            driver,
            repository: index.name.clone(),
            policy,
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
        .users
        .authenticate(&credentials.user, &credentials.password)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(unauthorized)?;
    let decision = state
        .authorization
        .authorize_scoped(&actor, Scope::AdministrationRead, &Resource::Operator);
    if decision.decision() != Decision::Allow {
        return Err(not_found());
    }
    Ok(actor)
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
}

/// A plan's strong validator: an administrator can present it with `If-Match` on a later apply to catch
/// a repository that changed under the preview.
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
