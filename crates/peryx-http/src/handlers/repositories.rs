//! The repository management API: versioned admin operations to create, inspect, list, update, and
//! disable repositories.
//!
//! Each mutation authenticates an administrator, validates the whole definition, and commits one
//! repository version. A repository is an opaque record keyed by a stable id and a unique route; the
//! store keeps the id fixed across a rename, so a reference never re-homes. Updates and state changes
//! carry an `If-Match` version so a stale write loses to the committed one and the winner's version
//! rides back on the `ETag`. A denied caller and a missing repository both read as `404`, so an
//! outsider cannot tell an inaccessible repository from an absent one.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use peryx_driver::authz::{Decision, ScopedDecision};
use peryx_driver::state::AppState;
use peryx_identity::{Resource, Scope, UserId, parse_basic};
use peryx_storage::meta::{
    CreateRepositoryError, NewRepository, RepositoryFieldError, RepositoryId, RepositoryQuery, RepositoryQueryError,
    RepositoryRecord, RepositoryState, RepositoryStateError, RepositoryUpdate, UpdateRepositoryError,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::response_security::ProtectedCachePolicy;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBody {
    route: String,
    display_name: String,
    ecosystem: String,
    #[serde(default)]
    definition: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateBody {
    display_name: String,
    #[serde(default)]
    definition: Value,
}

/// Filters for `GET /+repositories`: an optional state, a pagination cursor, and a page size.
#[derive(Deserialize)]
pub struct RepositoriesQuery {
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

/// `POST /+repositories` - create a repository. Requires administrator authority.
pub async fn create_repository(State(state): State<Arc<AppState>>, headers: HeaderMap, body: Bytes) -> Response {
    let (actor, _) = match administrator(&state, &headers, Scope::AdministrationWrite).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let create: CreateBody = match parse_json(&headers, &body) {
        Ok(create) => create,
        Err(rejection) => return rejection.into_response(),
    };
    let new = NewRepository {
        route: create.route,
        display_name: create.display_name,
        ecosystem: create.ecosystem,
        definition: create.definition,
        created_by: actor,
    };
    match state.meta.create_repository(new, now(&state)) {
        Ok(record) => created(&record),
        Err(CreateRepositoryError::DuplicateRoute { route }) => {
            conflict(&format!("a repository already serves route {route:?}"))
        }
        Err(CreateRepositoryError::Field(error)) => field_error(&error),
        Err(CreateRepositoryError::Store(_)) => unavailable(),
    }
}

/// `GET /+repositories` - list repositories, filtered and paginated. Requires administrator authority.
pub async fn list_repositories(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<RepositoriesQuery>,
) -> Response {
    if let Err(response) = administrator(&state, &headers, Scope::AdministrationRead).await {
        return response;
    }
    let repository_query = RepositoryQuery {
        state: query.state.as_deref().and_then(parse_state),
        cursor: query.cursor.as_deref().map(repository_id),
        limit: query.limit.unwrap_or_else(|| RepositoryQuery::default().limit),
    };
    match state.meta.list_repositories(&repository_query) {
        Ok(page) => json_no_store(StatusCode::OK, &json!(page)),
        Err(RepositoryQueryError::InvalidLimit) => problem(StatusCode::BAD_REQUEST, "limit must be between 1 and 100"),
        Err(RepositoryQueryError::Store(_)) => unavailable(),
    }
}

/// `GET /+repositories/{id}` - inspect one repository. Requires administrator authority.
pub async fn inspect_repository(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<RepositoryId>,
) -> Response {
    if let Err(response) = administrator(&state, &headers, Scope::AdministrationRead).await {
        return response;
    }
    match state.meta.repository(&id) {
        Ok(Some(record)) => record_response(StatusCode::OK, &record),
        Ok(None) => not_found(),
        Err(_) => unavailable(),
    }
}

/// `PUT /+repositories/{id}` - update a repository's display name and definition. Requires an
/// administrator and an `If-Match` version.
pub async fn update_repository(
    State(state): State<Arc<AppState>>,
    Path(id): Path<RepositoryId>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (actor, _) = match administrator(&state, &headers, Scope::AdministrationWrite).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let expected = match if_match(&headers) {
        Ok(version) => version,
        Err(rejection) => return rejection.into_response(),
    };
    let update: UpdateBody = match parse_json(&headers, &body) {
        Ok(update) => update,
        Err(rejection) => return rejection.into_response(),
    };
    let update = RepositoryUpdate {
        display_name: update.display_name,
        definition: update.definition,
    };
    match state.meta.update_repository(&id, expected, update, &actor, now(&state)) {
        Ok(record) => record_response(StatusCode::OK, &record),
        Err(UpdateRepositoryError::NotFound) => not_found(),
        Err(UpdateRepositoryError::VersionConflict { current }) => version_conflict(current),
        Err(UpdateRepositoryError::Field(error)) => field_error(&error),
        Err(UpdateRepositoryError::Store(_)) => unavailable(),
    }
}

/// `POST /+repositories/{id}/disable` - disable a repository. Requires an administrator and an
/// `If-Match` version.
pub async fn disable_repository(
    State(state): State<Arc<AppState>>,
    Path(id): Path<RepositoryId>,
    headers: HeaderMap,
) -> Response {
    set_enabled(&state, &id, &headers, false).await
}

/// `POST /+repositories/{id}/enable` - re-enable a disabled repository. Requires an administrator and
/// an `If-Match` version.
pub async fn enable_repository(
    State(state): State<Arc<AppState>>,
    Path(id): Path<RepositoryId>,
    headers: HeaderMap,
) -> Response {
    set_enabled(&state, &id, &headers, true).await
}

async fn set_enabled(state: &AppState, id: &RepositoryId, headers: &HeaderMap, enabled: bool) -> Response {
    let (actor, _) = match administrator(state, headers, Scope::AdministrationWrite).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let expected = match if_match(headers) {
        Ok(version) => version,
        Err(rejection) => return rejection.into_response(),
    };
    match state
        .meta
        .set_repository_enabled(id, expected, enabled, &actor, now(state))
    {
        Ok(record) => record_response(StatusCode::OK, &record),
        Err(RepositoryStateError::NotFound) => not_found(),
        Err(RepositoryStateError::VersionConflict { current }) => version_conflict(current),
        Err(RepositoryStateError::Store(_)) => unavailable(),
    }
}

/// Authenticate the caller and require `scope` over the operator resource. A missing or wrong
/// credential is `401`; a valid credential without the grant is `404`, so a denial and an absent
/// repository are indistinguishable to an outsider.
async fn administrator(
    state: &AppState,
    headers: &HeaderMap,
    scope: Scope,
) -> Result<(UserId, ScopedDecision), Response> {
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
    let decision = state.authorization.authorize_scoped(&actor, scope, &Resource::Operator);
    if decision.decision() != Decision::Allow {
        return Err(not_found());
    }
    Ok((actor, decision))
}

/// A lightweight rejection so a helper's `Err` stays small: a `Response` on the error side dwarfs a
/// `u64` or a parsed body, and returning one directly trips `clippy::result_large_err`.
struct Rejection {
    status: StatusCode,
    message: &'static str,
}

impl Rejection {
    fn into_response(self) -> Response {
        problem(self.status, self.message)
    }
}

fn parse_json<T: serde::de::DeserializeOwned>(headers: &HeaderMap, body: &Bytes) -> Result<T, Rejection> {
    if !is_json(headers) {
        return Err(Rejection {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            message: "expected application/json",
        });
    }
    serde_json::from_slice(body).map_err(|_| Rejection {
        status: StatusCode::UNPROCESSABLE_ENTITY,
        message: "malformed repository body",
    })
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"))
}

fn if_match(headers: &HeaderMap) -> Result<u64, Rejection> {
    let Some(value) = headers.get(header::IF_MATCH) else {
        return Err(Rejection {
            status: StatusCode::PRECONDITION_REQUIRED,
            message: "an If-Match version is required",
        });
    };
    value.to_str().ok().and_then(parse_etag).ok_or(Rejection {
        status: StatusCode::BAD_REQUEST,
        message: "If-Match must be a repository version",
    })
}

fn parse_state(value: &str) -> Option<RepositoryState> {
    serde_json::from_value(Value::String(value.to_owned())).ok()
}

fn repository_id(value: &str) -> RepositoryId {
    serde_json::from_value(Value::String(value.to_owned())).expect("any string is a valid repository id")
}

fn created(record: &RepositoryRecord) -> Response {
    let mut response = record_response(StatusCode::CREATED, record);
    let location = format!("/+repositories/{}", record.id);
    response.headers_mut().insert(
        header::LOCATION,
        HeaderValue::from_str(&location).expect("a repository id is a valid header value"),
    );
    response
}

fn record_response(status: StatusCode, record: &RepositoryRecord) -> Response {
    let mut response = respond(status, json!(record));
    response.headers_mut().insert(header::ETAG, etag(record.version));
    response
}

fn version_conflict(current: u64) -> Response {
    let mut response = respond(
        StatusCode::CONFLICT,
        json!({ "error": "repository version precondition failed", "current_version": current }),
    );
    response.headers_mut().insert(header::ETAG, etag(current));
    response
}

fn field_error(error: &RepositoryFieldError) -> Response {
    problem(StatusCode::UNPROCESSABLE_ENTITY, field_message(error))
}

const fn field_message(error: &RepositoryFieldError) -> &'static str {
    match error {
        RepositoryFieldError::EmptyRoute => "route must not be empty",
        RepositoryFieldError::RouteTooLong => "route is too long",
        RepositoryFieldError::EmptyDisplayName => "display name must not be empty",
        RepositoryFieldError::DisplayNameTooLong => "display name is too long",
        RepositoryFieldError::EmptyEcosystem => "ecosystem must not be empty",
        RepositoryFieldError::EcosystemTooLong => "ecosystem is too long",
    }
}

fn respond(status: StatusCode, body: Value) -> Response {
    let mut response = (status, axum::Json(body)).into_response();
    ProtectedCachePolicy::NoStore.apply(response.headers_mut());
    response
}

fn json_no_store(status: StatusCode, body: &Value) -> Response {
    respond(status, body.clone())
}

fn conflict(message: &str) -> Response {
    respond(StatusCode::CONFLICT, json!({ "error": message }))
}

fn not_found() -> Response {
    problem(StatusCode::NOT_FOUND, "no repository with that id")
}

fn problem(status: StatusCode, message: &str) -> Response {
    respond(status, json!({ "error": message }))
}

fn unauthorized() -> Response {
    let mut response = respond(
        StatusCode::UNAUTHORIZED,
        json!({ "error": "administrator credentials required" }),
    );
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"peryx-administration\""),
    );
    response
}

fn unavailable() -> Response {
    problem(StatusCode::SERVICE_UNAVAILABLE, "the repository store is unavailable")
}

fn etag(version: u64) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{version}\"")).expect("a version is a valid etag")
}

fn parse_etag(value: &str) -> Option<u64> {
    value.trim().trim_start_matches("W/").trim_matches('"').parse().ok()
}

fn now(state: &AppState) -> i64 {
    (state.clock)()
}
