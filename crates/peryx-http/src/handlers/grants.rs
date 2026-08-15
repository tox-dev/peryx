//! Callers cannot grant beyond their own authority. Versioned revocation preserves a binding reasserted
//! during a concurrent revoke.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use peryx_driver::authz::{
    DeleteGrantOutcome, RoleGrantFilter, RoleGrantQuery, RoleGrantQueryError, RoleGrantStoreError, StoredRoleGrant,
    role_grant_reach,
};
use peryx_driver::state::AppState;
use peryx_events::security::{RoleGrantChange, role_grant_change};
use peryx_identity::{GrantScope, Role, RoleGrant, UserId, can_manage_grants, parse_basic};

use crate::response_security::ProtectedCachePolicy;

const MAX_BODY: usize = 4 * 1024;
const DEFAULT_LIMIT: usize = 25;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateGrantBody {
    user: UserId,
    role: Role,
    scope: GrantScope,
}

#[derive(serde::Deserialize)]
pub struct GrantsQuery {
    user: Option<UserId>,
    resource: Option<String>,
    cursor: Option<String>,
    limit: Option<usize>,
}

pub async fn list_grants(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<GrantsQuery>,
) -> Response {
    let (_actor, grants) = match caller(&state, &headers).await {
        Ok(caller) => caller,
        Err(response) => return response,
    };
    let (filter, reach) = match (query.user, query.resource.as_deref()) {
        (Some(_), Some(_)) => return problem(StatusCode::BAD_REQUEST, "specify at most one of user or resource"),
        (None, Some(resource)) => match parse_reach(resource) {
            Some(scope) => (RoleGrantFilter::Resource(scope.clone()), scope),
            None => return problem(StatusCode::BAD_REQUEST, "invalid resource filter"),
        },
        (user, None) => (
            user.map_or(RoleGrantFilter::All, RoleGrantFilter::User),
            GrantScope::Server,
        ),
    };
    if !can_manage_grants(&grants, &reach) {
        return StatusCode::FORBIDDEN.into_response();
    }
    match state.serving.authorization.list_managed_grants(&RoleGrantQuery {
        filter,
        cursor: query.cursor,
        limit: query.limit.unwrap_or(DEFAULT_LIMIT),
    }) {
        Ok(page) => {
            let mut response = axum::Json(serde_json::json!({
                "grants": page.grants.iter().map(grant_json).collect::<Vec<_>>(),
                "next_cursor": page.next_cursor,
            }))
            .into_response();
            ProtectedCachePolicy::NoStore.apply(response.headers_mut());
            response
        }
        Err(RoleGrantQueryError::InvalidLimit) => problem(StatusCode::BAD_REQUEST, "limit must be between 1 and 100"),
        Err(RoleGrantQueryError::Store(_)) => unavailable(),
    }
}

pub async fn create_grant(State(state): State<Arc<AppState>>, request: Request<Body>) -> Response {
    let headers = request.headers().clone();
    if !is_json(&headers) {
        return problem(StatusCode::UNSUPPORTED_MEDIA_TYPE, "request body must be JSON");
    }
    let (actor, grants) = match caller(&state, &headers).await {
        Ok(caller) => caller,
        Err(response) => return response,
    };
    let body = match axum::body::to_bytes(request.into_body(), MAX_BODY).await {
        Ok(body) => match serde_json::from_slice::<CreateGrantBody>(&body) {
            Ok(body) => body,
            Err(_) => return problem(StatusCode::UNPROCESSABLE_ENTITY, "invalid request body"),
        },
        Err(_) => return problem(StatusCode::PAYLOAD_TOO_LARGE, "request body is too large"),
    };
    let target = RoleGrant::new(body.user, body.role, body.scope);
    if !target.is_effective() {
        return problem(StatusCode::UNPROCESSABLE_ENTITY, "role does not apply to the scope");
    }
    if !can_manage_grants(&grants, &target.scope) {
        record(&actor, RoleGrantChange::Grant, &target, "denied", "not_authorized");
        return StatusCode::FORBIDDEN.into_response();
    }
    match state
        .serving
        .authorization
        .create_managed_grant(&target, &actor, (state.serving.clock)())
    {
        Ok(outcome) => {
            record(
                &actor,
                RoleGrantChange::Grant,
                &outcome.record.grant,
                "allowed",
                if outcome.created { "created" } else { "updated" },
            );
            let status = if outcome.created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            grant_response(status, &outcome.record, true)
        }
        Err(RoleGrantStoreError::UnknownUser { .. }) => {
            record(&actor, RoleGrantChange::Grant, &target, "denied", "unknown_user");
            problem(StatusCode::UNPROCESSABLE_ENTITY, "user does not exist")
        }
        Err(RoleGrantStoreError::DisabledUser { .. }) => {
            record(&actor, RoleGrantChange::Grant, &target, "denied", "disabled_user");
            problem(StatusCode::UNPROCESSABLE_ENTITY, "user is disabled")
        }
        Err(RoleGrantStoreError::Store(_)) => unavailable(),
    }
}

pub async fn inspect_grant(State(state): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    let (_actor, grants) = match caller(&state, &headers).await {
        Ok(caller) => caller,
        Err(response) => return response,
    };
    if !administers(&id, &grants) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match state.serving.authorization.managed_grant(&id) {
        Ok(Some(stored)) => grant_response(StatusCode::OK, &stored, false),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => unavailable(),
    }
}

pub async fn revoke_grant(State(state): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    let (actor, grants) = match caller(&state, &headers).await {
        Ok(caller) => caller,
        Err(response) => return response,
    };
    let expected = match headers.get(header::IF_MATCH) {
        None => {
            return problem(
                StatusCode::PRECONDITION_REQUIRED,
                "revocation requires an If-Match precondition",
            );
        }
        Some(value) => match value.to_str().ok().and_then(parse_etag) {
            Some(version) => version,
            None => return problem(StatusCode::BAD_REQUEST, "invalid If-Match precondition"),
        },
    };
    if !administers(&id, &grants) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match state.serving.authorization.delete_managed_grant(&id, expected) {
        Ok(DeleteGrantOutcome::Removed(stored)) => {
            record(&actor, RoleGrantChange::Revoke, &stored.grant, "allowed", "removed");
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(DeleteGrantOutcome::PreconditionFailed { current }) => (
            StatusCode::PRECONDITION_FAILED,
            [(header::ETAG, etag(current))],
            axum::Json(serde_json::json!({"error": "grant version precondition failed"})),
        )
            .into_response(),
        Ok(DeleteGrantOutcome::NotFound) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => unavailable(),
    }
}

/// Whether the caller may administer the reach an identifier addresses. The reach comes from the
/// identifier itself, so authorization runs before any store read and a caller cannot tell an absent
/// grant apart from one it may not see.
fn administers(id: &str, grants: &[RoleGrant]) -> bool {
    role_grant_reach(id).is_some_and(|reach| can_manage_grants(grants, &reach))
}

async fn caller(state: &AppState, headers: &HeaderMap) -> Result<(UserId, Vec<RoleGrant>), Response> {
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
    let grants = state.serving.authorization.grants(&actor).map_err(|_| unavailable())?;
    Ok((actor, grants))
}

fn record(actor: &UserId, change: RoleGrantChange, grant: &RoleGrant, result: &'static str, reason: &'static str) {
    role_grant_change(
        Some(actor.as_str()),
        change,
        &grant.user,
        grant.role,
        &reach_label(&grant.scope),
        result,
        reason,
    );
}

fn grant_response(status: StatusCode, stored: &StoredRoleGrant, with_location: bool) -> Response {
    let id = stored.id();
    let mut response = (status, axum::Json(grant_json(stored))).into_response();
    let out = response.headers_mut();
    out.insert(header::ETAG, etag(stored.version));
    if with_location {
        out.insert(
            header::LOCATION,
            HeaderValue::from_str(&format!("/+grants/{id}")).expect("an opaque grant id is a valid location"),
        );
    }
    ProtectedCachePolicy::NoStore.apply(out);
    response
}

fn grant_json(stored: &StoredRoleGrant) -> serde_json::Value {
    serde_json::json!({
        "id": stored.id(),
        "user": stored.grant.user,
        "role": stored.grant.role,
        "scope": stored.grant.scope,
        "version": stored.version,
        "granted_by": stored.granted_by,
        "granted_at_unix": stored.granted_at_unix,
    })
}

fn parse_reach(value: &str) -> Option<GrantScope> {
    if value == "server" {
        return Some(GrantScope::Server);
    }
    let name = value.strip_prefix("repository/")?;
    (!name.is_empty()).then(|| GrantScope::Repository { name: name.to_owned() })
}

fn reach_label(scope: &GrantScope) -> String {
    match scope {
        GrantScope::Server => "server".to_owned(),
        GrantScope::Repository { name } => format!("repository/{name}"),
    }
}

fn parse_etag(value: &str) -> Option<u64> {
    value.trim().trim_matches('"').parse().ok()
}

fn etag(version: u64) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{version}\"")).expect("a version etag is a valid header value")
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"peryx-administration\"")],
    )
        .into_response()
}

fn unavailable() -> Response {
    problem(StatusCode::SERVICE_UNAVAILABLE, "grant service unavailable")
}

fn problem(status: StatusCode, message: &'static str) -> Response {
    (status, axum::Json(serde_json::json!({"error": message}))).into_response()
}
