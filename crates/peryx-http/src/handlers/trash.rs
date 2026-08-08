//! Operator inspection of soft-deleted artifacts across every ecosystem.
//!
//! One neutral query ([`AppState::query_trash`](peryx_driver::state::AppState::query_trash)) feeds both
//! this API and the web view. An administrator inspects every repository and sees the deleting actor; a
//! repository reader inspects one repository they can read, with actor details redacted by the role
//! filter. Responses never enter a shared cache.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Request, StatusCode, Uri, header};
use axum::response::{IntoResponse as _, Response};
use peryx_core::{Ecosystem, TrashState};
use peryx_driver::authz::{Decision, DenyReason, ScopedDecision};
use peryx_driver::state::AppState;
use peryx_driver::trash::{TrashItem, TrashPage, TrashQuery, TrashQueryError, TrashRef};
use peryx_identity::{Action, Resource, Scope, UserId, parse_basic};

use crate::response_security::{
    ClassifiedField, FieldClassification, ProtectedCachePolicy, ResponseAuthorization, filter_fields,
};

#[derive(Debug, serde::Deserialize)]
pub struct TrashListParams {
    repository: Option<String>,
    ecosystem: Option<String>,
    state: Option<String>,
    deadline_before: Option<i64>,
    cursor: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, serde::Deserialize)]
pub struct TrashInspectParams {
    ecosystem: String,
    repository: String,
    name: String,
    reference: Option<String>,
    digest: Option<String>,
}

/// `GET /+trash`: a filtered, paginated page of soft-deleted artifacts.
pub async fn list_trash(State(state): State<Arc<AppState>>, request: Request<Body>) -> Response {
    let (request, _) = request.into_parts();
    let mut response = list_trash_response(&state, &request.headers, &request.uri).await;
    ProtectedCachePolicy::NoStore.apply(response.headers_mut());
    response
}

/// `GET /+trash/record`: one soft-deleted artifact identified by its ecosystem, repository, and name.
pub async fn inspect_trash(State(state): State<Arc<AppState>>, request: Request<Body>) -> Response {
    let (request, _) = request.into_parts();
    let mut response = inspect_trash_response(&state, &request.headers, &request.uri).await;
    ProtectedCachePolicy::NoStore.apply(response.headers_mut());
    response
}

async fn list_trash_response(state: &AppState, headers: &HeaderMap, uri: &Uri) -> Response {
    let identity = match authenticate(state, headers).await {
        Ok(identity) => identity,
        Err(rejection) => return rejection.response(),
    };
    let Ok(Query(params)) = Query::<TrashListParams>::try_from_uri(uri) else {
        return invalid_query();
    };
    let (Ok(ecosystem), Ok(state_filter)) = (parse_ecosystem(params.ecosystem), parse_state(params.state)) else {
        return invalid_query();
    };
    let authorization = match authorize(state, headers, params.repository.as_deref(), &identity) {
        Ok(authorization) => authorization,
        Err(rejection) => return rejection.response(),
    };
    let query = TrashQuery {
        repository: authorization.repository,
        ecosystem,
        state: state_filter,
        deadline_before_unix: params.deadline_before,
        cursor: params.cursor,
        limit: params.limit.unwrap_or(25),
    };
    match state.query_trash(&query) {
        Ok(page) => trash_page(page, authorization.actor, authorization.response),
        Err(error) => trash_error_response(&error),
    }
}

async fn inspect_trash_response(state: &AppState, headers: &HeaderMap, uri: &Uri) -> Response {
    let identity = match authenticate(state, headers).await {
        Ok(identity) => identity,
        Err(rejection) => return rejection.response(),
    };
    let Ok(Query(params)) = Query::<TrashInspectParams>::try_from_uri(uri) else {
        return invalid_query();
    };
    let Ok(ecosystem) = params.ecosystem.parse::<Ecosystem>() else {
        return invalid_query();
    };
    let authorization = match authorize(state, headers, Some(&params.repository), &identity) {
        Ok(authorization) => authorization,
        Err(rejection) => return rejection.response(),
    };
    let reference = TrashRef {
        ecosystem,
        repository: params.repository,
        name: params.name,
        reference: params.reference,
        digest: params.digest,
    };
    match state.inspect_trash(&reference) {
        Ok(Some(item)) => trash_record(item, authorization.actor, authorization.response),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(error) => trash_error_response(&error),
    }
}

/// Whether the caller may see the actor that deleted each artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActorDetail {
    Full,
    Redacted,
}

#[derive(Debug)]
enum TrashIdentity {
    Local(UserId),
    LegacyToken,
}

#[derive(Debug)]
struct TrashAuthorization {
    repository: Option<String>,
    actor: ActorDetail,
    response: ResponseAuthorization,
}

#[derive(Debug, Clone, Copy)]
enum TrashRejection {
    Forbidden,
    NotFound,
    Unavailable,
    Unauthorized,
}

impl TrashRejection {
    fn response(self) -> Response {
        match self {
            Self::Forbidden => StatusCode::FORBIDDEN.into_response(),
            Self::NotFound => StatusCode::NOT_FOUND.into_response(),
            Self::Unavailable => unavailable(),
            Self::Unauthorized => unauthorized(),
        }
    }
}

impl From<super::LegacyDenied> for TrashRejection {
    fn from(denied: super::LegacyDenied) -> Self {
        match denied {
            super::LegacyDenied::Forbidden => Self::Forbidden,
            super::LegacyDenied::Unauthorized => Self::Unauthorized,
        }
    }
}

async fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<TrashIdentity, TrashRejection> {
    let credentials = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_basic)
        .ok_or(TrashRejection::Unauthorized)?;
    if credentials.user == "__token__" {
        return Ok(TrashIdentity::LegacyToken);
    }
    state
        .users
        .authenticate(&credentials.user, &credentials.password)
        .await
        .map_err(|_| TrashRejection::Unavailable)?
        .map(TrashIdentity::Local)
        .ok_or(TrashRejection::Unauthorized)
}

fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    route: Option<&str>,
    identity: &TrashIdentity,
) -> Result<TrashAuthorization, TrashRejection> {
    match identity {
        TrashIdentity::Local(actor) => authorize_local(state, actor, route),
        TrashIdentity::LegacyToken => authorize_legacy(state, headers, route),
    }
}

/// An administrator inspects any repository and sees actor details wherever they read; a repository
/// reader inspects one repository they can read, with actor details redacted. Actor visibility follows
/// the caller's role, so scoping an administrator to one repository does not redact it.
fn authorize_local(
    state: &AppState,
    actor: &UserId,
    route: Option<&str>,
) -> Result<TrashAuthorization, TrashRejection> {
    let Some(route) = route else {
        let authorization = state
            .authorization
            .authorize_scoped(actor, Scope::AdministrationRead, &Resource::Operator);
        require_permission(authorization)?;
        return Ok(TrashAuthorization {
            repository: None,
            actor: ActorDetail::Full,
            response: ResponseAuthorization::Scoped(authorization),
        });
    };
    let index = super::index_by_route(state, route).ok_or(TrashRejection::NotFound)?;
    let authorization =
        state
            .authorization
            .authorize_scoped(actor, Scope::RepositoryRead, &Resource::Repository(index.name.clone()));
    require_permission(authorization)?;
    let operator = state
        .authorization
        .authorize_scoped(actor, Scope::AdministrationRead, &Resource::Operator);
    Ok(TrashAuthorization {
        repository: Some(index.name.clone()),
        actor: if operator.decision().is_allowed() {
            ActorDetail::Full
        } else {
            ActorDetail::Redacted
        },
        response: ResponseAuthorization::Scoped(authorization),
    })
}

fn authorize_legacy(
    state: &AppState,
    headers: &HeaderMap,
    route: Option<&str>,
) -> Result<TrashAuthorization, TrashRejection> {
    let route = route.ok_or(TrashRejection::Unauthorized)?;
    let index = super::authorize_legacy_route(state, headers, route, Action::Write)?;
    Ok(TrashAuthorization {
        repository: Some(index.name.clone()),
        actor: ActorDetail::Redacted,
        response: ResponseAuthorization::Repository,
    })
}

const fn require_permission(authorization: ScopedDecision) -> Result<(), TrashRejection> {
    match authorization.decision() {
        Decision::Allow => Ok(()),
        Decision::Deny(DenyReason::NoGrant) => Err(TrashRejection::NotFound),
        Decision::Deny(DenyReason::StorageUnavailable) => Err(TrashRejection::Unavailable),
    }
}

fn parse_ecosystem(value: Option<String>) -> Result<Option<Ecosystem>, ()> {
    value
        .map(|value| value.parse::<Ecosystem>().map_err(|_| ()))
        .transpose()
}

fn parse_state(value: Option<String>) -> Result<Option<TrashState>, ()> {
    value
        .map(|value| value.parse::<TrashState>().map_err(|_| ()))
        .transpose()
}

#[derive(serde::Serialize)]
struct TrashRecordResponse {
    ecosystem: &'static str,
    repository: String,
    name: String,
    reference: Option<String>,
    digest: Option<String>,
    reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    actor: Option<String>,
    deleted_at_unix: i64,
    deadline_unix: i64,
    state: TrashState,
    restorable: bool,
}

impl TrashRecordResponse {
    fn new(item: TrashItem, actor: ActorDetail) -> Self {
        let record = item.record;
        Self {
            ecosystem: record.ecosystem.as_str(),
            repository: record.repository,
            name: record.name,
            reference: record.reference,
            digest: record.digest,
            reason: record.reason,
            actor: matches!(actor, ActorDetail::Full).then_some(record.actor).flatten(),
            deleted_at_unix: record.deleted_at_unix,
            deadline_unix: item.deadline_unix,
            state: item.state,
            restorable: item.restorable,
        }
    }
}

fn trash_page(page: TrashPage, actor: ActorDetail, authorization: ResponseAuthorization) -> Response {
    let mut records = Vec::with_capacity(page.items.len());
    for item in page.items {
        records.push(TrashRecordResponse::new(item, actor));
    }
    axum::Json(serde_json::Value::Object(
        filter_fields(
            authorization,
            [
                ClassifiedField::new("trash", FieldClassification::Repository, serde_json::json!(records)),
                ClassifiedField::new(
                    "next_cursor",
                    FieldClassification::Repository,
                    serde_json::json!(page.next_cursor),
                ),
            ],
        )
        .expect("authorization passed before the trash query"),
    ))
    .into_response()
}

fn trash_record(item: TrashItem, actor: ActorDetail, authorization: ResponseAuthorization) -> Response {
    let record = TrashRecordResponse::new(item, actor);
    axum::Json(serde_json::Value::Object(
        filter_fields(
            authorization,
            [ClassifiedField::new(
                "record",
                FieldClassification::Repository,
                serde_json::json!(record),
            )],
        )
        .expect("authorization passed before the trash lookup"),
    ))
    .into_response()
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"peryx-trash\"")],
    )
        .into_response()
}

fn unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(serde_json::json!({"error": "trash inspection service unavailable"})),
    )
        .into_response()
}

fn invalid_query() -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(serde_json::json!({"error": "invalid trash query"})),
    )
        .into_response()
}

/// Keep validation failures actionable without exposing storage details.
#[must_use]
pub fn trash_error_response(error: &TrashQueryError) -> Response {
    let (status, message) = match error {
        TrashQueryError::InvalidLimit | TrashQueryError::InvalidCursor | TrashQueryError::RepositoryTooLong => {
            (StatusCode::BAD_REQUEST, error.to_string())
        }
        TrashQueryError::Store(_) => (StatusCode::INTERNAL_SERVER_ERROR, "trash query failed".to_owned()),
    };
    (status, axum::Json(serde_json::json!({"error": message}))).into_response()
}
