//! Cancellation signals are process-local, so remote callers use the serving node. Denials return the
//! same `404` as missing runs.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{Extensions, HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use peryx_driver::authz::{Decision, DenyReason};
use peryx_driver::jobs::CancelJobRun;
use peryx_driver::state::AppState;
use peryx_identity::{Resource, Scope, parse_basic};

/// Requests cooperative cancellation for a local run.
///
/// A delivered signal returns `202 Accepted` while the run unwinds. A non-local or finished run returns
/// `409 Conflict`. Unknown and unauthorized runs return `404 Not Found`.
pub async fn cancel_job(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    extensions: Extensions,
    Path(id): Path<String>,
) -> Response {
    if let Err(rejection) = authorize_administrator(&state, &headers, &extensions).await {
        return rejection.response();
    }
    match state.serving.job_attempts.cancel(&id) {
        Ok(CancelJobRun::Requested) => StatusCode::ACCEPTED.into_response(),
        Ok(CancelJobRun::Finished) => conflict("job run already finished"),
        Ok(CancelJobRun::Unavailable) => conflict("job run is not running on this node"),
        Ok(CancelJobRun::Missing) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => Rejection::Unavailable.response(),
    }
}

/// Authenticate a local user and require the server-wide administration-write scope. A missing grant
/// answers the same `404` a missing run does, so a denial cannot confirm the endpoint to a probe.
async fn authorize_administrator(
    state: &AppState,
    headers: &HeaderMap,
    extensions: &Extensions,
) -> Result<(), Rejection> {
    let credentials = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_basic)
        .ok_or(Rejection::Unauthorized)?;
    let actor = state
        .serving
        .users
        .authenticate_request(extensions, &credentials)
        .await
        .map_err(|_| Rejection::Unavailable)?
        .ok_or(Rejection::Unauthorized)?;
    match state
        .serving
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
