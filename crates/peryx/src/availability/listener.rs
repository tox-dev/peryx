//! The private, administrator-authenticated availability control listener.
//!
//! Availability controls never share the public package routes. A `dc` or `ha` node binds this router
//! on its own socket (see [`AvailabilityListenerConfig`]), authenticates every request against the same
//! identity store the package API uses, and admits a principal holding the server-wide administration
//! scope over [`Resource::Operator`]. Single-node `none` builds none of this, so the control plane costs
//! a single-writer process nothing.
//!
//! This module assembles and authorizes the router; the process entrypoint owns the socket, TLS
//! termination, and graceful drain. A read-only status endpoint reports the node's availability posture
//! behind the [`Scope::AdministrationRead`] scope, and a command endpoint submits membership and transfer
//! commands to the ownership consensus group behind the [`Scope::AdministrationWrite`] scope. Middleware
//! authenticates once and each route authorizes the scope its operation needs.
//!
//! [`AvailabilityListenerConfig`]: crate::config::AvailabilityListenerConfig

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{HeaderMap, HeaderName, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse as _, Response};
use axum::routing::{delete, get, post};
use axum::{Extension, Json, Router};
use peryx_driver::authz::Decision;
use peryx_driver::state::{AppState, ControlCommand, ControlError};
use peryx_ha_distributed::{AuthorityKey, DatacenterId, TransferAudit, TransferRequest};
use peryx_identity::{Resource, Scope, UserId, parse_basic};
use serde::Deserialize;
use serde_json::json;

use crate::availability::{TransferCancelError, TransferCoordinator, TransferDriveError, TransferRunError};
use crate::config::{AvailabilityConfig, ReplicationConfig};

/// The request header a client stamps to make a command idempotent: a repeat carrying the same value
/// reads back the first committed receipt rather than minting a second command.
static IDEMPOTENCY_KEY: HeaderName = HeaderName::from_static("idempotency-key");

/// The availability control protocol version this node advertises to a client of the listener.
///
/// A client pins the versions it understands and refuses an incompatible peer rather than guessing a
/// wire shape. Version 2 adds the membership and transfer command surface to the read-only version 1.
pub const AVAILABILITY_PROTOCOL_VERSION: u32 = 2;

/// The largest control request body the listener reads, in bytes. The status surface carries none; the
/// bound stands so a later command endpoint cannot be handed an unbounded body on the control plane.
const MAX_CONTROL_BODY_BYTES: usize = 64 * 1024;

/// A node's availability posture: the mode it runs and the authority role it holds.
///
/// The status endpoint reports it, built from the resolved [`AvailabilityConfig`], so it exists only for
/// `dc` and `ha`.
#[derive(Clone, Copy)]
pub struct AvailabilityPosture {
    mode: &'static str,
    role: &'static str,
}

impl AvailabilityPosture {
    /// The posture a `dc` or `ha` node reports, or `None` under single-node `none`, which runs no
    /// listener and therefore has no posture to expose.
    #[must_use]
    pub fn from_config(availability: &AvailabilityConfig) -> Option<Self> {
        let role = match availability.replication()? {
            ReplicationConfig::Primary { .. } => "writer",
            ReplicationConfig::Replica { .. } => "replica",
        };
        Some(Self {
            mode: availability.mode().as_str(),
            role,
        })
    }
}

/// The state the listener's router and its authentication middleware share.
#[derive(Clone)]
struct ListenerState {
    app: Arc<AppState>,
    posture: AvailabilityPosture,
    coordinator: Arc<TransferCoordinator>,
}

/// Assemble the availability control router: an administrator-authenticated, version-prefixed surface
/// bounded to [`MAX_CONTROL_BODY_BYTES`] and kept apart from the public package routes.
///
/// The caller binds the returned router on the private [`AvailabilityListenerConfig`] socket. Every
/// matched route runs behind [`authenticate`]; an unmatched path answers `404` without touching the
/// identity store, so an unauthenticated caller cannot probe the surface.
///
/// [`AvailabilityListenerConfig`]: crate::config::AvailabilityListenerConfig
pub fn router(app: Arc<AppState>, posture: AvailabilityPosture, coordinator: Arc<TransferCoordinator>) -> Router {
    let state = ListenerState {
        app,
        posture,
        coordinator,
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

/// Authenticate a request and hand the resolved actor to the route, which authorizes the scope its
/// operation needs. A missing or invalid credential is `401`; an identity store that cannot answer is
/// `503`. Authorization is left to the handler so a read route and a command route gate different scopes.
async fn authenticate(State(state): State<ListenerState>, mut request: Request, next: Next) -> Response {
    match authenticate_actor(&state.app, request.headers()).await {
        Ok(actor) => {
            tracing::info!(%actor, path = %request.uri().path(), "availability control request authenticated");
            request.extensions_mut().insert(actor);
            next.run(request).await
        }
        Err(response) => response,
    }
}

/// Resolve the request's Basic credential to an actor, reusing the package API's identity store so the
/// control plane holds no second user database.
async fn authenticate_actor(app: &AppState, headers: &HeaderMap) -> Result<UserId, Response> {
    let credentials = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_basic)
        .ok_or_else(unauthorized)?;
    app.users
        .authenticate(&credentials.user, &credentials.password)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(unauthorized)
}

/// The `403` response when `actor` lacks `scope` over the operator resource, or `None` when it holds it.
/// The listener resolves the actor once and each route gates the scope its operation needs.
fn scope_denied(app: &AppState, actor: &UserId, scope: Scope) -> Option<Response> {
    if app
        .authorization
        .authorize_scoped(actor, scope, &Resource::Operator)
        .decision()
        == Decision::Allow
    {
        None
    } else {
        Some(forbidden())
    }
}

/// Report the node's availability posture: the advertised protocol version, its mode and authority role,
/// whether it currently serves read-only, and, when this node runs an ownership consensus group, that
/// group's leader, term, and voter membership. Requires the administration read scope.
async fn status(State(state): State<ListenerState>, Extension(actor): Extension<UserId>) -> Response {
    if let Some(denied) = scope_denied(&state.app, &actor, Scope::AdministrationRead) {
        return denied;
    }
    let mut body = serde_json::Map::from_iter([
        ("protocol_version".to_owned(), json!(AVAILABILITY_PROTOCOL_VERSION)),
        ("mode".to_owned(), json!(state.posture.mode)),
        ("role".to_owned(), json!(state.posture.role)),
        ("read_only".to_owned(), json!(state.app.read_only)),
    ]);
    if let Some(group) = state.app.ownership_authority() {
        let status = serde_json::to_value(group.cluster_status()).expect("cluster status serializes to JSON");
        body.insert("consensus".to_owned(), status);
    }
    if let Some(plane) = state.app.control_plane() {
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

/// Submit a membership or transfer command to the ownership consensus group.
///
/// The command commits through the Raft log, never a direct store write. It requires the
/// [`Scope::AdministrationWrite`] scope, dedupes on an optional `Idempotency-Key` so a retry across a
/// leader loss reads one committed receipt, and answers with the committed term and index. A node that
/// runs no consensus group has nothing to command and answers `503`.
async fn command(
    State(state): State<ListenerState>,
    Extension(actor): Extension<UserId>,
    headers: HeaderMap,
    Json(command): Json<ControlCommand>,
) -> Response {
    if let Some(denied) = scope_denied(&state.app, &actor, Scope::AdministrationWrite) {
        return denied;
    }
    let Some(plane) = state.app.control_plane() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "this node runs no ownership consensus group",
        )
            .into_response();
    };
    let key = headers.get(&IDEMPOTENCY_KEY).and_then(|value| value.to_str().ok());
    match plane.execute(&actor.to_string(), key, command).await {
        Ok(receipt) => (StatusCode::OK, [(header::CACHE_CONTROL, "no-store")], Json(receipt)).into_response(),
        Err(error) => command_error(&error),
    }
}

/// Map a control failure to its HTTP response: a leadership or reachability failure is retryable `503`, an
/// invalid transition or an idempotency key reused for a different command is a `409`, and a saturated
/// concurrency bound is `429`.
fn command_error(error: &ControlError) -> Response {
    let status = match error {
        ControlError::NotLeader { .. } | ControlError::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
        ControlError::Invalid(_) | ControlError::KeyReuse => StatusCode::CONFLICT,
        ControlError::Overloaded => StatusCode::TOO_MANY_REQUESTS,
    };
    (status, error.to_string()).into_response()
}

/// A requested fenced transfer: which authority moves, between which datacenters, and why.
///
/// The barrier the move waits on is the node's own current metadata serial rather than a client value, so
/// a caller cannot ask the move to commit before the target has replicated this node's writes. The actor
/// comes from the authenticated principal, not the body, so the audit records who truly ordered it.
#[derive(Deserialize)]
struct TransferBody {
    authority: String,
    source: String,
    target: String,
    reason: String,
}

/// Start a fenced authority transfer: fence it behind this node's current metadata serial, then drive it
/// to a sealed audit through the coordinator.
///
/// The move commits through the ownership consensus group once the target has caught up, so it requires
/// the [`Scope::AdministrationWrite`] scope and a node that runs a group and control plane; a node that
/// runs neither answers `503`. A transfer already running for the authority is a `409`, a target that
/// never reaches the barrier within the budget is a `504`, and a committed move answers with its audit.
async fn start_transfer(
    State(state): State<ListenerState>,
    Extension(actor): Extension<UserId>,
    headers: HeaderMap,
    Json(body): Json<TransferBody>,
) -> Response {
    if let Some(denied) = scope_denied(&state.app, &actor, Scope::AdministrationWrite) {
        return denied;
    }
    let (Some(control), Some(ownership)) = (state.app.control_plane(), state.app.ownership_authority()) else {
        return no_consensus();
    };
    let barrier = match state.app.meta.current_serial() {
        Ok(serial) => serial,
        Err(error) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("read the transfer barrier: {error}"),
            )
                .into_response();
        }
    };
    let request = TransferRequest {
        authority: AuthorityKey(body.authority),
        source: DatacenterId(body.source),
        target: DatacenterId(body.target),
        actor: actor.to_string(),
        reason: body.reason,
        barrier,
    };
    let key = headers.get(&IDEMPOTENCY_KEY).and_then(|value| value.to_str().ok());
    match state
        .coordinator
        .run(request, control, ownership, &state.app.meta, key)
        .await
    {
        Ok(audit) => transfer_committed(&audit),
        Err(error) => run_error(&error),
    }
}

/// Cancel a fenced transfer that has not committed, so an operator can abandon a move whose target never
/// caught up. Requires the [`Scope::AdministrationWrite`] scope. An unknown authority is `404`, a move
/// that already committed is a `409`, and a cancelled move answers `204`.
async fn cancel_transfer(
    State(state): State<ListenerState>,
    Extension(actor): Extension<UserId>,
    Path(authority): Path<String>,
) -> Response {
    if let Some(denied) = scope_denied(&state.app, &actor, Scope::AdministrationWrite) {
        return denied;
    }
    match state.coordinator.cancel(&authority).await {
        Ok(()) => (StatusCode::NO_CONTENT, [(header::CACHE_CONTROL, "no-store")]).into_response(),
        Err(error) => cancel_error(&error),
    }
}

/// The `200` response carrying a sealed transfer audit: who moved what, from where to where, the barrier
/// it waited on, and the epoch and log index the committed move minted.
fn transfer_committed(audit: &TransferAudit) -> Response {
    let body = json!({
        "authority": audit.authority.0,
        "source": audit.source.0,
        "target": audit.target.0,
        "actor": audit.actor,
        "reason": audit.reason,
        "barrier": audit.barrier,
        "epoch": audit.epoch.0,
        "commit_index": audit.commit_index,
    });
    (StatusCode::OK, [(header::CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

/// The `503` a node without a consensus group and control plane answers, having nothing to commit a move
/// through.
fn no_consensus() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "this node runs no ownership consensus group",
    )
        .into_response()
}

/// Map a transfer drive failure to its HTTP response: an already-running transfer is a `409`, a target
/// that never caught up in budget is a retryable `504`, and a drive failure resolves against its cause.
fn run_error(error: &TransferRunError) -> Response {
    match error {
        TransferRunError::Busy(_) => (StatusCode::CONFLICT, error.to_string()).into_response(),
        TransferRunError::BarrierNotReached => (StatusCode::GATEWAY_TIMEOUT, error.to_string()).into_response(),
        TransferRunError::Drive(drive) => drive_error(drive),
    }
}

/// Map a drive failure to its HTTP response: an unreachable target frontier or a failed persist is a
/// retryable `503`, a plan the move refused is a `409`, and a refused commit resolves as a command does.
fn drive_error(error: &TransferDriveError) -> Response {
    match error {
        TransferDriveError::Commit(control) => command_error(control),
        TransferDriveError::Plan(plan) => (StatusCode::CONFLICT, plan.to_string()).into_response(),
        TransferDriveError::Frontier(_) | TransferDriveError::Persist(_) => {
            (StatusCode::SERVICE_UNAVAILABLE, error.to_string()).into_response()
        }
    }
}

/// Map a transfer cancel failure to its HTTP response: an unknown authority is `404`, a committed move
/// that can no longer be cancelled is `409`.
fn cancel_error(error: &TransferCancelError) -> Response {
    let status = match error {
        TransferCancelError::Unknown(_) => StatusCode::NOT_FOUND,
        TransferCancelError::AlreadyCommitted(_) => StatusCode::CONFLICT,
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
    (StatusCode::SERVICE_UNAVAILABLE, "identity store unavailable").into_response()
}
