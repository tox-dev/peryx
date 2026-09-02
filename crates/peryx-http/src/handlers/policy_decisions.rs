use std::sync::Arc;

use axum::Extension;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{Extensions, HeaderMap, Request, StatusCode, Uri, header};
use axum::response::{IntoResponse as _, Response};
use peryx_driver::authz::{Decision, DenyReason, ScopedDecision};
use peryx_driver::http_services::{
    HttpDomainServices, PolicyDecisionItem, PolicyDecisionPage, PolicyDecisionQuery, PolicyDecisionQueryError,
    PolicyInputGeneration,
};
use peryx_driver::state::AppState;
use peryx_identity::{Action, Resource, Scope, UserId, parse_basic};
use peryx_policy::PolicyDecisionState;

use crate::response_security::{
    ClassifiedField, FieldClassification, ProtectedCachePolicy, ResponseAuthorization, filter_fields,
};
use peryx_driver::route_auth::AdminRealm;

#[derive(Debug, serde::Deserialize)]
pub struct PolicyDecisionsQuery {
    repository: Option<String>,
    resource: Option<String>,
    state: Option<PolicyDecisionState>,
    rule: Option<String>,
    source: Option<String>,
    from: Option<i64>,
    to: Option<i64>,
    cursor: Option<String>,
    limit: Option<usize>,
}

pub async fn policy_decisions(
    State(state): State<Arc<AppState>>,
    Extension(services): Extension<HttpDomainServices>,
    request: Request<Body>,
) -> Response {
    let (request, _) = request.into_parts();
    let mut response =
        policy_decisions_response(&state, &services, &request.headers, &request.extensions, &request.uri).await;
    ProtectedCachePolicy::NoStore.apply(response.headers_mut());
    response
}

async fn policy_decisions_response(
    state: &AppState,
    services: &HttpDomainServices,
    headers: &HeaderMap,
    extensions: &Extensions,
    uri: &Uri,
) -> Response {
    let identity = match authenticate(state, headers, extensions).await {
        Ok(identity) => identity,
        Err(rejection) => return rejection.response(),
    };
    let Ok(Query(query)) = Query::<PolicyDecisionsQuery>::try_from_uri(uri) else {
        return invalid_query();
    };
    let mut query = PolicyDecisionQuery {
        repository: query.repository,
        resource: query.resource,
        state: query.state,
        rule: query.rule,
        source: query.source,
        evaluated_from_unix: query.from,
        evaluated_to_unix: query.to,
        cursor: query.cursor,
        limit: query.limit.unwrap_or(25),
    };
    if let Err(error) = query.validate() {
        return policy_decision_error_response(&error);
    }
    let authorization = match authorize_query(state, headers, query.repository.as_deref(), &identity) {
        Ok(authorization) => authorization,
        Err(rejection) => return rejection.response(),
    };
    query.repository = authorization.repository;
    match services.policy_decisions().query(&query) {
        Ok(page) => policy_decision_page(page, authorization.response),
        Err(error) => policy_decision_error_response(&error),
    }
}

#[derive(Debug)]
enum PolicyDecisionIdentity {
    Local(UserId),
    EcosystemCredential,
}

#[derive(Debug)]
struct PolicyDecisionAuthorization {
    repository: Option<String>,
    response: ResponseAuthorization,
}

#[derive(Debug, Clone, Copy)]
enum PolicyDecisionRejection {
    Forbidden,
    NotFound,
    Unavailable,
    Unauthorized,
}

impl PolicyDecisionRejection {
    fn response(self) -> Response {
        match self {
            Self::Forbidden => StatusCode::FORBIDDEN.into_response(),
            Self::NotFound => StatusCode::NOT_FOUND.into_response(),
            Self::Unavailable => unavailable(),
            Self::Unauthorized => unauthorized(),
        }
    }
}

impl From<super::EcosystemCredentialDenied> for PolicyDecisionRejection {
    fn from(denied: super::EcosystemCredentialDenied) -> Self {
        match denied {
            super::EcosystemCredentialDenied::Forbidden => Self::Forbidden,
            super::EcosystemCredentialDenied::Unauthorized => Self::Unauthorized,
        }
    }
}

async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
    extensions: &Extensions,
) -> Result<PolicyDecisionIdentity, PolicyDecisionRejection> {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or(PolicyDecisionRejection::Unauthorized)?;
    if state.recognizes_index_credential(authorization) {
        return Ok(PolicyDecisionIdentity::EcosystemCredential);
    }
    let credentials = parse_basic(authorization).ok_or(PolicyDecisionRejection::Unauthorized)?;
    state
        .serving
        .users
        .authenticate_request(extensions, &credentials)
        .await
        .map_err(|_| PolicyDecisionRejection::Unavailable)?
        .map(PolicyDecisionIdentity::Local)
        .ok_or(PolicyDecisionRejection::Unauthorized)
}

fn authorize_query(
    state: &AppState,
    headers: &HeaderMap,
    route: Option<&str>,
    identity: &PolicyDecisionIdentity,
) -> Result<PolicyDecisionAuthorization, PolicyDecisionRejection> {
    match identity {
        PolicyDecisionIdentity::Local(actor) => authorize_local_repository(state, actor, route),
        PolicyDecisionIdentity::EcosystemCredential => authorize_ecosystem_repository(state, headers, route),
    }
}

fn authorize_local_repository(
    state: &AppState,
    actor: &UserId,
    route: Option<&str>,
) -> Result<PolicyDecisionAuthorization, PolicyDecisionRejection> {
    let Some(route) = route else {
        let authorization =
            state
                .serving
                .authorization
                .authorize_scoped(actor, Scope::AdministrationRead, &Resource::Operator);
        require_permission(authorization)?;
        return Ok(PolicyDecisionAuthorization {
            repository: None,
            response: ResponseAuthorization::Scoped(authorization),
        });
    };
    let index = super::index_by_route(state, route).ok_or(PolicyDecisionRejection::NotFound)?;
    let authorization = state.serving.authorization.authorize_scoped(
        actor,
        Scope::RepositoryRead,
        &Resource::Repository(index.name.clone()),
    );
    require_permission(authorization)?;
    Ok(PolicyDecisionAuthorization {
        repository: Some(index.name.clone()),
        response: ResponseAuthorization::Scoped(authorization),
    })
}

fn authorize_ecosystem_repository(
    state: &AppState,
    headers: &HeaderMap,
    route: Option<&str>,
) -> Result<PolicyDecisionAuthorization, PolicyDecisionRejection> {
    let route = route.ok_or(PolicyDecisionRejection::Unauthorized)?;
    let index = super::authorize_ecosystem_credential(state, headers, route, Action::Write)?;
    Ok(PolicyDecisionAuthorization {
        repository: Some(index.name.clone()),
        response: ResponseAuthorization::Repository,
    })
}

const fn require_permission(authorization: ScopedDecision) -> Result<(), PolicyDecisionRejection> {
    match authorization.decision() {
        Decision::Allow => Ok(()),
        Decision::Deny(DenyReason::NoGrant) => Err(PolicyDecisionRejection::NotFound),
        Decision::Deny(DenyReason::StorageUnavailable) => Err(PolicyDecisionRejection::Unavailable),
    }
}

fn policy_decision_page(page: PolicyDecisionPage, authorization: ResponseAuthorization) -> Response {
    let decisions = page
        .decisions
        .into_iter()
        .map(PolicyDecisionResponse::from)
        .collect::<Vec<_>>();
    axum::Json(serde_json::Value::Object(
        filter_fields(
            authorization,
            [
                ClassifiedField::new(
                    "decisions",
                    FieldClassification::Repository,
                    serde_json::json!(decisions),
                ),
                ClassifiedField::new(
                    "next_cursor",
                    FieldClassification::Repository,
                    serde_json::json!(page.next_cursor),
                ),
            ],
        )
        .expect("authorization passed before the policy query"),
    ))
    .into_response()
}

#[derive(serde::Serialize)]
struct PolicyDecisionResponse {
    id: String,
    repository: String,
    resource: String,
    group: Option<String>,
    artifact: Option<String>,
    source: Option<String>,
    action: peryx_policy::PolicyAction,
    state: PolicyDecisionState,
    rule: Option<String>,
    reason: Option<String>,
    evaluated_at_unix: i64,
    input_generation: PolicyInputGeneration,
    next_eligible_at_unix: Option<i64>,
    fresh: bool,
}

impl From<PolicyDecisionItem> for PolicyDecisionResponse {
    fn from(decision: PolicyDecisionItem) -> Self {
        let record = decision.record;
        Self {
            id: record.id.to_string(),
            repository: record.repository,
            resource: record.resource,
            group: record.group,
            artifact: record.artifact,
            source: record.source,
            action: record.action,
            state: record.state,
            rule: record.rule,
            reason: record.reason,
            evaluated_at_unix: record.evaluated_at_unix,
            input_generation: record.input_generation,
            next_eligible_at_unix: record.next_eligible_at_unix,
            fresh: decision.fresh,
        }
    }
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, AdminRealm::PolicyDecisions.challenge())],
    )
        .into_response()
}

fn unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(serde_json::json!({"error": "policy decision service unavailable"})),
    )
        .into_response()
}

fn invalid_query() -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(serde_json::json!({"error": "invalid policy decision query"})),
    )
        .into_response()
}

/// Return validation details for client errors; redact store failures.
#[must_use]
pub fn policy_decision_error_response(error: &PolicyDecisionQueryError) -> Response {
    let (status, message) = match error {
        PolicyDecisionQueryError::InvalidLimit
        | PolicyDecisionQueryError::InvalidCursor
        | PolicyDecisionQueryError::FilterTooLong { .. } => (StatusCode::BAD_REQUEST, error.to_string()),
        PolicyDecisionQueryError::Store(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "policy decision query failed".to_owned(),
        ),
    };
    (status, axum::Json(serde_json::json!({"error": message}))).into_response()
}
