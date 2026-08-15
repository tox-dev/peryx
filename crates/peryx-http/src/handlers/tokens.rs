//! Callers may manage only token scopes covered by their grants. Denials return `404` to hide unreachable
//! repositories and tokens. Secrets appear only in create and rotate responses, which are never cached.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use peryx_driver::authz::{Decision, DenyReason, ScopedDecision};
use peryx_driver::state::AppState;
use peryx_driver::tokens::{CreateScopedToken, ScopedTokenQuery, ScopedTokenQueryError, ScopedTokenRecord};
use peryx_identity::{Action, GrantScope, Resource, Scope, TokenId, TokenName, UserId, parse_basic};

use crate::response_security::ProtectedCachePolicy;

/// The largest create request body accepted, generous for a name and a handful of actions.
const MAX_BODY_BYTES: usize = 4 * 1024;

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateTokenBody {
    name: String,
    #[serde(default)]
    repository: Option<String>,
    actions: Vec<Action>,
    #[serde(default)]
    expires_at: Option<i64>,
}

#[derive(Debug, serde::Deserialize)]
pub struct ListTokensQuery {
    repository: Option<String>,
    cursor: Option<String>,
    limit: Option<usize>,
}

pub async fn create_token(State(state): State<Arc<AppState>>, request: Request<Body>) -> Response {
    let (parts, body) = request.into_parts();
    let actor = match authenticate(&state, &parts.headers).await {
        Ok(actor) => actor,
        Err(rejection) => return rejection.response(),
    };
    let body = match read_json_body(&parts.headers, body).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    let Ok(name) = TokenName::new(&body.name) else {
        return problem(StatusCode::BAD_REQUEST, "invalid token name");
    };
    if body.actions.is_empty() {
        return problem(StatusCode::BAD_REQUEST, "at least one action is required");
    }
    let actions: BTreeSet<Action> = body.actions.into_iter().collect();
    let now = (state.serving.clock)();
    if body.expires_at.is_some_and(|expiry| expiry <= now) {
        return problem(StatusCode::BAD_REQUEST, "expiry must be in the future");
    }
    let reach = match reach_of(&state, body.repository.as_deref()) {
        Ok(reach) => reach,
        Err(rejection) => return rejection.response(),
    };
    if let Err(rejection) = authorize_grant(&state, &actor, &reach, &actions) {
        return rejection.response();
    }
    match state.serving.tokens.create(
        CreateScopedToken {
            name,
            reach,
            actions,
            expires_at: body.expires_at,
            created_by: actor,
        },
        now,
    ) {
        Ok((record, secret)) => minted_response(StatusCode::CREATED, &record, secret.expose()),
        Err(_) => unavailable(),
    }
}

pub async fn list_tokens(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<ListTokensQuery>,
) -> Response {
    let actor = match authenticate(&state, &headers).await {
        Ok(actor) => actor,
        Err(rejection) => return rejection.response(),
    };
    let reach = match reach_of(&state, query.repository.as_deref()) {
        Ok(reach) => reach,
        Err(rejection) => return rejection.response(),
    };
    if let Err(rejection) = authorize_manage(&state, &actor, &reach) {
        return rejection.response();
    }
    match state.serving.tokens.list(&ScopedTokenQuery {
        reach,
        cursor: query.cursor.map(TokenId::new),
        limit: query.limit.unwrap_or(25),
    }) {
        Ok(page) => no_store(axum::Json(page).into_response()),
        Err(ScopedTokenQueryError::InvalidLimit) => problem(StatusCode::BAD_REQUEST, "invalid limit"),
        Err(ScopedTokenQueryError::Store(_)) => unavailable(),
    }
}

pub async fn inspect_token(State(state): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    let (_actor, record) = match load_and_authorize(&state, &headers, &TokenId::new(id)).await {
        Ok(authorized) => authorized,
        Err(rejection) => return rejection.response(),
    };
    record.map_or_else(
        || StatusCode::NOT_FOUND.into_response(),
        |record| token_response(StatusCode::OK, &record),
    )
}

pub async fn rotate_token(State(state): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    let id = TokenId::new(id);
    let (actor, _record) = match load_and_authorize(&state, &headers, &id).await {
        Ok(authorized) => authorized,
        Err(rejection) => return rejection.response(),
    };
    match state.serving.tokens.rotate(&id, &actor) {
        Ok(Some((record, secret))) => minted_response(StatusCode::OK, &record, secret.expose()),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => unavailable(),
    }
}

pub async fn revoke_token(State(state): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<String>) -> Response {
    let id = TokenId::new(id);
    let (actor, _record) = match load_and_authorize(&state, &headers, &id).await {
        Ok(authorized) => authorized,
        Err(rejection) => return rejection.response(),
    };
    match state.serving.tokens.revoke(&id, &actor, (state.serving.clock)()) {
        Ok(Some(outcome)) => token_response(StatusCode::OK, outcome.record()),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => unavailable(),
    }
}

/// A caller that lacks authority over an existing token's reach is refused here. An absent token is not:
/// it resolves to `None` so the caller's operation answers `404` from its own store step, which keeps a
/// revoked or never-created token indistinguishable from one the caller may not reach.
async fn load_and_authorize(
    state: &AppState,
    headers: &HeaderMap,
    id: &TokenId,
) -> Result<(UserId, Option<ScopedTokenRecord>), Rejection> {
    let actor = authenticate(state, headers).await?;
    let Some(record) = state.serving.tokens.inspect(id).map_err(|_| Rejection::Unavailable)? else {
        return Ok((actor, None));
    };
    authorize_manage(state, &actor, &record.reach)?;
    Ok((actor, Some(record)))
}

async fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<UserId, Rejection> {
    let credentials = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_basic)
        .ok_or(Rejection::Unauthorized)?;
    state
        .serving
        .users
        .authenticate(&credentials.user, &credentials.password)
        .await
        .map_err(|_| Rejection::Unavailable)?
        .ok_or(Rejection::Unauthorized)
}

fn reach_of(state: &AppState, repository: Option<&str>) -> Result<GrantScope, Rejection> {
    repository.map_or(Ok(GrantScope::Server), |route| {
        state
            .serving
            .indexes
            .iter()
            .find(|index| index.route == route)
            .map(|index| GrantScope::Repository {
                name: index.name.clone(),
            })
            .ok_or(Rejection::NotFound)
    })
}

fn authorize_grant(
    state: &AppState,
    actor: &UserId,
    reach: &GrantScope,
    actions: &BTreeSet<Action>,
) -> Result<(), Rejection> {
    match reach {
        GrantScope::Server => require_permission(state.serving.authorization.authorize_scoped(
            actor,
            Scope::AdministrationWrite,
            &Resource::Operator,
        )),
        GrantScope::Repository { name } => {
            let resource = Resource::Repository(name.clone());
            for action in actions {
                require_permission(state.serving.authorization.authorize_scoped(
                    actor,
                    repository_scope(*action),
                    &resource,
                ))?;
            }
            Ok(())
        }
    }
}

fn authorize_manage(state: &AppState, actor: &UserId, reach: &GrantScope) -> Result<(), Rejection> {
    let (scope, resource) = match reach {
        GrantScope::Server => (Scope::AdministrationWrite, Resource::Operator),
        GrantScope::Repository { name } => (Scope::RepositoryWrite, Resource::Repository(name.clone())),
    };
    require_permission(state.serving.authorization.authorize_scoped(actor, scope, &resource))
}

const fn repository_scope(action: Action) -> Scope {
    match action {
        Action::Read => Scope::RepositoryRead,
        Action::Write => Scope::RepositoryWrite,
        Action::Delete => Scope::RepositoryDelete,
    }
}

const fn require_permission(decision: ScopedDecision) -> Result<(), Rejection> {
    match decision.decision() {
        Decision::Allow => Ok(()),
        Decision::Deny(DenyReason::NoGrant) => Err(Rejection::NotFound),
        Decision::Deny(DenyReason::StorageUnavailable) => Err(Rejection::Unavailable),
    }
}

/// A refusal a synchronous helper reports, mapped to its HTTP answer at the handler boundary. Kept small
/// so a helper does not return a full response by value.
#[derive(Debug, Clone, Copy)]
enum Rejection {
    Unauthorized,
    NotFound,
    Unavailable,
}

impl Rejection {
    fn response(self) -> Response {
        match self {
            Self::Unauthorized => unauthorized(),
            Self::NotFound => StatusCode::NOT_FOUND.into_response(),
            Self::Unavailable => unavailable(),
        }
    }
}

async fn read_json_body(headers: &HeaderMap, body: Body) -> Result<CreateTokenBody, Response> {
    if !is_json(headers) {
        return Err(problem(StatusCode::UNSUPPORTED_MEDIA_TYPE, "request body must be JSON"));
    }
    let bytes = axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| problem(StatusCode::PAYLOAD_TOO_LARGE, "request body is too large"))?;
    serde_json::from_slice(&bytes).map_err(|_| problem(StatusCode::UNPROCESSABLE_ENTITY, "invalid request body"))
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
}

fn minted_response(status: StatusCode, record: &ScopedTokenRecord, secret: &str) -> Response {
    no_store(
        (
            status,
            axum::Json(serde_json::json!({"token": record, "secret": secret})),
        )
            .into_response(),
    )
}

fn token_response(status: StatusCode, record: &ScopedTokenRecord) -> Response {
    no_store((status, axum::Json(record)).into_response())
}

fn no_store(mut response: Response) -> Response {
    ProtectedCachePolicy::NoStore.apply(response.headers_mut());
    response
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"peryx-administration\"")],
    )
        .into_response()
}

fn unavailable() -> Response {
    problem(StatusCode::SERVICE_UNAVAILABLE, "token service unavailable")
}

fn problem(status: StatusCode, message: &'static str) -> Response {
    (status, axum::Json(serde_json::json!({"error": message}))).into_response()
}
