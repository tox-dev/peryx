//! Each mutation authenticates an administrator, validates the whole definition, and commits one
//! repository version. A repository is an opaque record keyed by a stable id and a unique route; the
//! store keeps the id fixed across a rename, so a reference never re-homes. Updates and state changes
//! carry an `If-Match` version so a stale write loses to the committed one and the winner's version
//! rides back on the `ETag`. A denied caller and a missing repository both read as `404`, so an
//! outsider cannot tell an inaccessible repository from an absent one.

use std::sync::Arc;

use axum::Extension;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{Extensions, HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use peryx_driver::authz::{Decision, ScopedDecision};
use peryx_driver::http_services::{
    CreateRepositoryError, HttpDomainServices, NewRepository, RepositoryFieldError, RepositoryId, RepositoryQuery,
    RepositoryQueryError, RepositoryRecord, RepositoryState, RepositoryStateError, RepositoryUpdate,
    UpdateRepositoryError, VersionPrecondition,
};
use peryx_driver::state::AppState;
use peryx_identity::{Resource, Scope, UserId, parse_basic};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::response_security::ProtectedCachePolicy;

use super::IfMatchError;

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

pub async fn create_repository(
    State(state): State<Arc<AppState>>,
    Extension(services): Extension<HttpDomainServices>,
    headers: HeaderMap,
    extensions: Extensions,
    body: Bytes,
) -> Response {
    let (actor, _) = match administrator(&state, &headers, &extensions, Scope::AdministrationWrite).await {
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
    match services.repositories().create(new, now(&state)) {
        Ok(record) => created(&record),
        Err(CreateRepositoryError::DuplicateRoute { route }) => {
            conflict(&format!("a repository already serves route {route:?}"))
        }
        Err(CreateRepositoryError::Field(error)) => field_error(&error),
        Err(CreateRepositoryError::Store(_)) => unavailable(),
    }
}

pub async fn list_repositories(
    State(state): State<Arc<AppState>>,
    Extension(services): Extension<HttpDomainServices>,
    headers: HeaderMap,
    extensions: Extensions,
    Query(query): Query<RepositoriesQuery>,
) -> Response {
    if let Err(response) = administrator(&state, &headers, &extensions, Scope::AdministrationRead).await {
        return response;
    }
    let repository_query = RepositoryQuery {
        state: match query.state.as_deref().map(parse_state).transpose() {
            Ok(state) => state,
            Err(rejection) => return rejection.into_response(),
        },
        cursor: query.cursor.as_deref().map(repository_id),
        limit: query.limit.unwrap_or_else(|| RepositoryQuery::default().limit),
    };
    match services.repositories().list(&repository_query) {
        Ok(page) => json_no_store(StatusCode::OK, &json!(page)),
        Err(RepositoryQueryError::InvalidLimit) => problem(StatusCode::BAD_REQUEST, "limit must be between 1 and 100"),
        Err(RepositoryQueryError::Store(_)) => unavailable(),
    }
}

pub async fn inspect_repository(
    State(state): State<Arc<AppState>>,
    Extension(services): Extension<HttpDomainServices>,
    headers: HeaderMap,
    extensions: Extensions,
    Path(id): Path<RepositoryId>,
) -> Response {
    if let Err(response) = administrator(&state, &headers, &extensions, Scope::AdministrationRead).await {
        return response;
    }
    match services.repositories().inspect(&id) {
        Ok(Some(record)) => record_response(StatusCode::OK, &record),
        Ok(None) => not_found(),
        Err(_) => unavailable(),
    }
}

pub async fn update_repository(
    State(state): State<Arc<AppState>>,
    Extension(services): Extension<HttpDomainServices>,
    Path(id): Path<RepositoryId>,
    headers: HeaderMap,
    extensions: Extensions,
    body: Bytes,
) -> Response {
    let (actor, _) = match administrator(&state, &headers, &extensions, Scope::AdministrationWrite).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let precondition = match version_precondition(&headers) {
        Ok(precondition) => precondition,
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
    match services
        .repositories()
        .update(&id, precondition, update, &actor, now(&state))
    {
        Ok(record) => record_response(StatusCode::OK, &record),
        Err(UpdateRepositoryError::PreconditionFailed { current }) => precondition_failed(current),
        Err(UpdateRepositoryError::Field(error)) => field_error(&error),
        Err(UpdateRepositoryError::Store(_)) => unavailable(),
    }
}

pub async fn disable_repository(
    State(state): State<Arc<AppState>>,
    Extension(services): Extension<HttpDomainServices>,
    Path(id): Path<RepositoryId>,
    headers: HeaderMap,
    extensions: Extensions,
) -> Response {
    set_enabled(&state, &services, &id, &headers, &extensions, false).await
}

pub async fn enable_repository(
    State(state): State<Arc<AppState>>,
    Extension(services): Extension<HttpDomainServices>,
    Path(id): Path<RepositoryId>,
    headers: HeaderMap,
    extensions: Extensions,
) -> Response {
    set_enabled(&state, &services, &id, &headers, &extensions, true).await
}

async fn set_enabled(
    state: &AppState,
    services: &HttpDomainServices,
    id: &RepositoryId,
    headers: &HeaderMap,
    extensions: &Extensions,
    enabled: bool,
) -> Response {
    let (actor, _) = match administrator(state, headers, extensions, Scope::AdministrationWrite).await {
        Ok(actor) => actor,
        Err(response) => return response,
    };
    let precondition = match version_precondition(headers) {
        Ok(precondition) => precondition,
        Err(rejection) => return rejection.into_response(),
    };
    match services
        .repositories()
        .set_enabled(id, precondition, enabled, &actor, now(state))
    {
        Ok(record) => record_response(StatusCode::OK, &record),
        Err(RepositoryStateError::PreconditionFailed { current }) => precondition_failed(current),
        Err(RepositoryStateError::Store(_)) => unavailable(),
    }
}

/// Authenticate the caller and require `scope` over the operator resource. A missing or wrong
/// credential is `401`; a valid credential without the grant is `404`, so a denial and an absent
/// repository are indistinguishable to an outsider.
async fn administrator(
    state: &AppState,
    headers: &HeaderMap,
    extensions: &Extensions,
    scope: Scope,
) -> Result<(UserId, ScopedDecision), Response> {
    let credentials = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_basic)
        .ok_or_else(unauthorized)?;
    let actor = state
        .serving
        .users
        .authenticate_request(extensions, &credentials)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(unauthorized)?;
    let decision = state
        .serving
        .authorization
        .authorize_scoped(&actor, scope, &Resource::Operator);
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
    if !super::is_json(headers) {
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

fn version_precondition(headers: &HeaderMap) -> Result<VersionPrecondition, Rejection> {
    super::if_match(headers).map_err(|error| match error {
        IfMatchError::Missing => Rejection {
            status: StatusCode::PRECONDITION_REQUIRED,
            message: "an If-Match version is required",
        },
        IfMatchError::Malformed => Rejection {
            status: StatusCode::BAD_REQUEST,
            message: "If-Match must contain entity tags or *",
        },
    })
}

fn parse_state(value: &str) -> Result<RepositoryState, Rejection> {
    match value {
        "enabled" => Ok(RepositoryState::Enabled),
        "disabled" => Ok(RepositoryState::Disabled),
        _ => Err(Rejection {
            status: StatusCode::BAD_REQUEST,
            message: "state must be enabled or disabled",
        }),
    }
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

fn precondition_failed(current: Option<u64>) -> Response {
    let mut body = json!({ "error": "repository version precondition failed" });
    if let Some(current) = current {
        body["current_version"] = json!(current);
    }
    let mut response = respond(StatusCode::PRECONDITION_FAILED, body);
    if let Some(current) = current {
        response.headers_mut().insert(header::ETAG, etag(current));
    }
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

fn now(state: &AppState) -> i64 {
    (state.serving.clock)()
}
