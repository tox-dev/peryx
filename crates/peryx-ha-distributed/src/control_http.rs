use std::sync::Arc;

use crate::{
    AuthorityKey, DatacenterId, DistributedMode, TransferAudit, TransferCancelError, TransferCoordinator,
    TransferDriveError, TransferRequest, TransferRunError,
};
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{HeaderMap, HeaderName, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse as _, Response};
use axum::routing::{delete, get, post};
use axum::{Extension, Json, Router};
use peryx_ha::{
    ControlActor, ControlAuthorizer, ControlCommand, ControlError, ControlExecutor, ControlPermission,
    OwnershipAuthority,
};
use peryx_storage::meta::MetaStore;
use serde::Deserialize;
use serde_json::json;

/// Reusing this header value replays the first committed receipt.
static IDEMPOTENCY_KEY: HeaderName = HeaderName::from_static("idempotency-key");

/// The availability control protocol version this node advertises to a client of the listener.
///
/// A client pins the versions it understands and refuses an incompatible peer rather than guessing a
/// wire shape. Version 2 adds the membership and transfer command surface to the read-only version 1.
pub const AVAILABILITY_PROTOCOL_VERSION: u32 = 2;

const MAX_CONTROL_BODY_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvailabilityPostureRole {
    Writer,
    Replica,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AvailabilityPosture {
    mode: &'static str,
    role: &'static str,
}

impl AvailabilityPosture {
    #[must_use]
    pub(crate) const fn new(mode: DistributedMode, role: AvailabilityPostureRole) -> Self {
        Self {
            mode: mode.as_str(),
            role: match role {
                AvailabilityPostureRole::Writer => "writer",
                AvailabilityPostureRole::Replica => "replica",
            },
        }
    }
}

#[derive(Clone)]
struct ListenerState {
    authorizer: Arc<dyn ControlAuthorizer>,
    posture: AvailabilityPosture,
    read_only: bool,
    meta: MetaStore,
    control: Option<Arc<dyn ControlExecutor>>,
    ownership: Option<Arc<dyn OwnershipAuthority>>,
    coordinator: Arc<TransferCoordinator>,
}

pub struct ControlHttpContext {
    pub authorizer: Arc<dyn ControlAuthorizer>,
    pub posture: AvailabilityPosture,
    pub read_only: bool,
    pub meta: MetaStore,
    pub control: Option<Arc<dyn ControlExecutor>>,
    pub ownership: Option<Arc<dyn OwnershipAuthority>>,
    pub coordinator: Arc<TransferCoordinator>,
}

/// Matched routes require authentication. Unmatched paths return `404` without querying identity storage.
pub fn availability_router(context: ControlHttpContext) -> Router {
    let state = ListenerState {
        authorizer: context.authorizer,
        posture: context.posture,
        read_only: context.read_only,
        meta: context.meta,
        control: context.control,
        ownership: context.ownership,
        coordinator: context.coordinator,
    };
    Router::new()
        .route("/availability/v1/status", get(status))
        .route("/availability/v1/commands", post(command))
        .route("/availability/v1/transfers", post(start_transfer))
        .route("/availability/v1/transfers/{authority}", delete(cancel_transfer))
        .route_layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .layer(DefaultBodyLimit::max(MAX_CONTROL_BODY_BYTES))
        .with_state(state)
}

/// Returns `401` for invalid credentials and `503` when identity storage fails. Handlers authorize their
/// required scope.
async fn authenticate(State(state): State<ListenerState>, mut request: Request, next: Next) -> Response {
    match authenticate_actor(state.authorizer.as_ref(), request.headers()).await {
        Ok(actor) => {
            tracing::info!(%actor, path = %request.uri().path(), "availability control request authenticated");
            request.extensions_mut().insert(actor);
            next.run(request).await
        }
        Err(response) => response,
    }
}

/// Uses the public API identity store to avoid a second control-plane credential database.
async fn authenticate_actor(authorizer: &dyn ControlAuthorizer, headers: &HeaderMap) -> Result<ControlActor, Response> {
    let authorization = headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok());
    authorizer
        .authenticate(authorization)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(unauthorized)
}

fn scope_denied(state: &ListenerState, actor: &ControlActor, permission: ControlPermission) -> Option<Response> {
    if state.authorizer.allows(actor, permission) {
        None
    } else {
        Some(forbidden())
    }
}

async fn status(State(state): State<ListenerState>, Extension(actor): Extension<ControlActor>) -> Response {
    if let Some(denied) = scope_denied(&state, &actor, ControlPermission::Read) {
        return denied;
    }
    let mut body = serde_json::Map::from_iter([
        ("protocol_version".to_owned(), json!(AVAILABILITY_PROTOCOL_VERSION)),
        ("mode".to_owned(), json!(state.posture.mode)),
        ("role".to_owned(), json!(state.posture.role)),
        ("read_only".to_owned(), json!(state.read_only)),
    ]);
    if let Some(group) = &state.ownership {
        let status = serde_json::to_value(group.cluster_status()).expect("cluster status serializes to JSON");
        body.insert("consensus".to_owned(), status);
    }
    if let Some(plane) = &state.control {
        let metrics = serde_json::to_value(plane.metrics()).expect("command metrics serialize to JSON");
        body.insert("commands".to_owned(), metrics);
    }
    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::Value::Object(body)),
    )
        .into_response()
}

/// Commits through the Raft log and deduplicates retries by optional idempotency key.
/// Requires write permission; nodes without ownership consensus return `503`.
async fn command(
    State(state): State<ListenerState>,
    Extension(actor): Extension<ControlActor>,
    headers: HeaderMap,
    Json(command): Json<ControlCommand>,
) -> Response {
    if let Some(denied) = scope_denied(&state, &actor, ControlPermission::Write) {
        return denied;
    }
    let Some(plane) = &state.control else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "this node runs no ownership consensus group",
        )
            .into_response();
    };
    let key = headers.get(&IDEMPOTENCY_KEY).and_then(|value| value.to_str().ok());
    match plane.execute(actor.as_str(), key, command).await {
        Ok(receipt) => (StatusCode::OK, [(header::CACHE_CONTROL, "no-store")], Json(receipt)).into_response(),
        Err(error) => command_error(&error),
    }
}

fn command_error(error: &ControlError) -> Response {
    let status = match error {
        ControlError::NotLeader { .. } | ControlError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
        ControlError::Invalid(_) | ControlError::KeyReuse => StatusCode::CONFLICT,
        ControlError::Overloaded => StatusCode::TOO_MANY_REQUESTS,
    };
    (status, error.to_string()).into_response()
}

/// The server supplies the barrier and actor so clients cannot weaken replication or forge audit identity.
#[derive(Deserialize)]
struct TransferBody {
    authority: String,
    source: String,
    target: String,
    reason: String,
}

/// Uses the node's current metadata serial as the barrier. Busy transfers return `409`; barrier timeout
/// returns `504`; nodes without ownership consensus return `503`.
async fn start_transfer(
    State(state): State<ListenerState>,
    Extension(actor): Extension<ControlActor>,
    headers: HeaderMap,
    Json(body): Json<TransferBody>,
) -> Response {
    if let Some(denied) = scope_denied(&state, &actor, ControlPermission::Write) {
        return denied;
    }
    let (Some(control), Some(ownership)) = (&state.control, &state.ownership) else {
        return no_consensus();
    };
    let barrier = match state.meta.current_serial() {
        Ok(serial) => serial,
        Err(error) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("read the transfer barrier: {error}"),
            )
                .into_response();
        }
    };
    // Consensus needs an identity before submission to retain recovery state for each move.
    let request = TransferRequest {
        id: headers
            .get(&IDEMPOTENCY_KEY)
            .and_then(|value| value.to_str().ok())
            .map_or_else(|| format!("transfer-{}", uuid::Uuid::new_v4().simple()), str::to_owned),
        authority: AuthorityKey(body.authority),
        source: DatacenterId(body.source),
        target: DatacenterId(body.target),
        actor: actor.as_str().to_owned(),
        reason: body.reason,
        barrier,
    };
    match state
        .coordinator
        .run(request, control.as_ref(), ownership, &state.meta)
        .await
    {
        Ok(audit) => transfer_committed(&audit),
        Err(error) => run_error(&error),
    }
}

/// Rejects cancellation after commit, resolving a commit the coordinator no longer retains against the
/// persisted audit. Unknown authorities return `404`; committed transfers return `409`.
async fn cancel_transfer(
    State(state): State<ListenerState>,
    Extension(actor): Extension<ControlActor>,
    Path(authority): Path<String>,
) -> Response {
    if let Some(denied) = scope_denied(&state, &actor, ControlPermission::Write) {
        return denied;
    }
    match state.coordinator.cancel(&authority, &state.meta).await {
        Ok(()) => (StatusCode::NO_CONTENT, [(header::CACHE_CONTROL, "no-store")]).into_response(),
        Err(error) => cancel_error(&error),
    }
}

fn transfer_committed(audit: &TransferAudit) -> Response {
    let body = json!({
        "id": audit.id,
        "authority": audit.authority.0,
        "source": audit.source.0,
        "target": audit.target.0,
        "actor": audit.actor,
        "reason": audit.reason,
        "barrier": audit.barrier,
        "epoch": audit.epoch.0,
        "commit_term": audit.commit_term,
        "commit_index": audit.commit_index,
    });
    (StatusCode::OK, [(header::CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

fn no_consensus() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "this node runs no ownership consensus group",
    )
        .into_response()
}

fn run_error(error: &TransferRunError) -> Response {
    match error {
        TransferRunError::Busy(_) => (StatusCode::CONFLICT, error.to_string()).into_response(),
        TransferRunError::BarrierNotReached => (StatusCode::GATEWAY_TIMEOUT, error.to_string()).into_response(),
        TransferRunError::Drive(drive) => drive_error(drive),
    }
}

fn drive_error(error: &TransferDriveError) -> Response {
    match error {
        TransferDriveError::Commit(control) => command_error(control),
        TransferDriveError::Plan(plan) => (StatusCode::CONFLICT, plan.to_string()).into_response(),
        TransferDriveError::Frontier(_)
        | TransferDriveError::Persist(_)
        | TransferDriveError::ProjectionPending { .. }
        | TransferDriveError::Recover(_)
        | TransferDriveError::Unsealed(_) => (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response(),
    }
}

fn cancel_error(error: &TransferCancelError) -> Response {
    let status = match error {
        TransferCancelError::Unknown(_) => StatusCode::NOT_FOUND,
        TransferCancelError::AlreadyCommitted(_) => StatusCode::CONFLICT,
        TransferCancelError::Durable(..) => StatusCode::SERVICE_UNAVAILABLE,
    };
    (status, error.to_string()).into_response()
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"peryx-availability\"")],
    )
        .into_response()
}

fn forbidden() -> Response {
    (
        StatusCode::FORBIDDEN,
        "availability control requires the administration scope",
    )
        .into_response()
}

fn unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::CACHE_CONTROL, "no-store")],
        "identity store unavailable",
    )
        .into_response()
}

#[cfg(test)]
#[path = "../tests/unit/control_http_tests.rs"]
mod tests;
