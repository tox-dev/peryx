//! Cancel a running node-local job.
//!
//! The durable job-run history the `job` CLI lists lives in every node's store, but a run's cooperative
//! cancellation signal lives only in the process running it, so no CLI in a separate process can reach
//! it. This endpoint reaches the live node's attempt control, letting an administrator stop a stuck
//! node-local job without restarting the node. A denial answers the same `404` a missing run does, so
//! it cannot confirm a run id to a probe.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use peryx_driver::authz::{Decision, DenyReason};
use peryx_driver::jobs::CancelJobRun;
use peryx_driver::state::AppState;
use peryx_identity::{Resource, Scope, parse_basic};

/// `POST /+jobs/{id}/cancel`: signal a running node-local job run to stop.
///
/// Cancellation is cooperative: the run observes the signal and unwinds within its grace period, so a
/// delivered signal answers `202 Accepted` rather than a completed stop. A run this node is not
/// currently running answers `409 Conflict`, and an unknown run - or a caller without the
/// administration-write scope - a `404`.
pub async fn cancel_job(State(state): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    if let Err(rejection) = authorize_administrator(&state, &headers).await {
        return rejection.response();
    }
    match state.job_attempts.cancel(&id) {
        Ok(CancelJobRun::Requested) => StatusCode::ACCEPTED.into_response(),
        Ok(CancelJobRun::Finished) => conflict("job run already finished"),
        Ok(CancelJobRun::Unavailable) => conflict("job run is not running on this node"),
        Ok(CancelJobRun::Missing) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => Rejection::Unavailable.response(),
    }
}

/// Authenticate a local user and require the server-wide administration-write scope. A missing grant
/// answers the same `404` a missing run does, so a denial cannot confirm the endpoint to a probe.
async fn authorize_administrator(state: &AppState, headers: &HeaderMap) -> Result<(), Rejection> {
    let credentials = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_basic)
        .ok_or(Rejection::Unauthorized)?;
    let actor = state
        .users
        .authenticate(&credentials.user, &credentials.password)
        .await
        .map_err(|_| Rejection::Unavailable)?
        .ok_or(Rejection::Unauthorized)?;
    match state
        .authorization
        .authorize_scoped(&actor, Scope::AdministrationWrite, &Resource::Operator)
        .decision()
    {
        Decision::Allow => Ok(()),
        Decision::Deny(DenyReason::NoGrant) => Err(Rejection::NotFound),
        Decision::Deny(DenyReason::StorageUnavailable) => Err(Rejection::Unavailable),
    }
}

enum Rejection {
    NotFound,
    Unavailable,
    Unauthorized,
}

impl Rejection {
    fn response(self) -> Response {
        match self {
            Self::NotFound => StatusCode::NOT_FOUND.into_response(),
            Self::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({"error": "job control unavailable"})),
            )
                .into_response(),
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Basic realm=\"peryx-administration\"")],
            )
                .into_response(),
        }
    }
}

fn conflict(message: &'static str) -> Response {
    (StatusCode::CONFLICT, axum::Json(serde_json::json!({"error": message}))).into_response()
}
