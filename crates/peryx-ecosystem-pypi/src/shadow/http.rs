//! The query replays a virtual repository's resolution of a project and returns the selected candidate for each
//! filename plus every candidate a member shadowed. Repository authorization gates the whole response,
//! so a caller who cannot read the repository learns nothing - not a member name, filename, or digest.
//! The candidates carry no upstream URLs. Responses never enter a shared cache.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Request, StatusCode, Uri, header};
use axum::response::{IntoResponse as _, Response};
use peryx_driver::authz::{Decision, DenyReason, ScopedDecision};
use peryx_driver::{AppState, HttpRoutes};
use peryx_identity::{Action, Resource, Scope, UserId, parse_basic};
use peryx_policy::PolicyDecisionState;

use peryx_http::response_security::{
    ClassifiedField, FieldClassification, ProtectedCachePolicy, ResponseAuthorization, filter_fields,
};

use super::inspect::{InspectedCandidate, ShadowInspection, inspect_shadowed};
use super::{ShadowQuery, ShadowQueryError, ShadowReason};

pub struct ShadowRoutes;

impl HttpRoutes for ShadowRoutes {
    fn routes(&self) -> axum::Router<Arc<AppState>> {
        axum::Router::new()
            .route("/+shadow/candidates", axum::routing::get(shadow_candidates))
            .route("/admin/shadow", axum::routing::get(shadow_admin))
    }
}

async fn shadow_admin() -> axum::response::Html<&'static str> {
    axum::response::Html(include_str!("shadow.html"))
}

#[derive(Debug, serde::Deserialize)]
pub struct ShadowParams {
    repository: String,
    project: String,
    cursor: Option<String>,
    limit: Option<usize>,
}

pub async fn shadow_candidates(State(state): State<Arc<AppState>>, request: Request<Body>) -> Response {
    let (request, _) = request.into_parts();
    let mut response = shadow_candidates_response(&state, &request.headers, &request.uri).await;
    ProtectedCachePolicy::NoStore.apply(response.headers_mut());
    response
}

async fn shadow_candidates_response(state: &AppState, headers: &HeaderMap, uri: &Uri) -> Response {
    let identity = match authenticate(state, headers).await {
        Ok(identity) => identity,
        Err(rejection) => return rejection.response(),
    };
    let Ok(Query(params)) = Query::<ShadowParams>::try_from_uri(uri) else {
        return invalid_query();
    };
    let authorization = match authorize(state, headers, &params.repository, &identity) {
        Ok(authorization) => authorization,
        Err(rejection) => return rejection.response(),
    };
    let query = ShadowQuery {
        repository: authorization.repository,
        project: params.project,
        cursor: params.cursor,
        limit: params.limit.unwrap_or(25),
    };
    match inspect_shadowed(state, &query) {
        Ok(page) => shadow_page(page, authorization.response),
        Err(error) => shadow_error_response(&error),
    }
}

#[derive(Debug)]
enum ShadowIdentity {
    Local(UserId),
    EcosystemCredential,
}

#[derive(Debug)]
struct ShadowAuthorization {
    repository: String,
    response: ResponseAuthorization,
}

#[derive(Debug, Clone, Copy)]
enum ShadowRejection {
    Forbidden,
    NotFound,
    Unavailable,
    Unauthorized,
}

impl ShadowRejection {
    fn response(self) -> Response {
        match self {
            Self::Forbidden => StatusCode::FORBIDDEN.into_response(),
            Self::NotFound => StatusCode::NOT_FOUND.into_response(),
            Self::Unavailable => unavailable(),
            Self::Unauthorized => unauthorized(),
        }
    }
}

async fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<ShadowIdentity, ShadowRejection> {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or(ShadowRejection::Unauthorized)?;
    if state.recognizes_index_credential(authorization) {
        return Ok(ShadowIdentity::EcosystemCredential);
    }
    let credentials = parse_basic(authorization).ok_or(ShadowRejection::Unauthorized)?;
    state
        .serving
        .users
        .authenticate(&credentials.user, &credentials.password)
        .await
        .map_err(|_| ShadowRejection::Unavailable)?
        .map(ShadowIdentity::Local)
        .ok_or(ShadowRejection::Unauthorized)
}

fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    route: &str,
    identity: &ShadowIdentity,
) -> Result<ShadowAuthorization, ShadowRejection> {
    match identity {
        ShadowIdentity::Local(actor) => authorize_local(state, actor, route),
        ShadowIdentity::EcosystemCredential => authorize_ecosystem(state, headers, route),
    }
}

/// A caller who can read the repository may inspect how it resolves a project; the operator role, which
/// carries no repository access, cannot.
fn authorize_local(state: &AppState, actor: &UserId, route: &str) -> Result<ShadowAuthorization, ShadowRejection> {
    let index = state
        .serving
        .indexes
        .iter()
        .find(|index| index.route == route)
        .ok_or(ShadowRejection::NotFound)?;
    let authorization = state.serving.authorization.authorize_scoped(
        actor,
        Scope::RepositoryRead,
        &Resource::Repository(index.name.clone()),
    );
    require_permission(authorization)?;
    Ok(ShadowAuthorization {
        repository: index.name.clone(),
        response: ResponseAuthorization::Scoped(authorization),
    })
}

fn authorize_ecosystem(
    state: &AppState,
    headers: &HeaderMap,
    route: &str,
) -> Result<ShadowAuthorization, ShadowRejection> {
    let index = state
        .serving
        .indexes
        .iter()
        .find(|index| index.route == route)
        .ok_or(ShadowRejection::Unauthorized)?;
    let authorization = headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok());
    state
        .authorize_index_credential(index, authorization, Action::Write)
        .map_err(|denial| match denial {
            peryx_identity::Denial::Forbidden => ShadowRejection::Forbidden,
            peryx_identity::Denial::Unavailable | peryx_identity::Denial::Unauthenticated => {
                ShadowRejection::Unauthorized
            }
        })?;
    Ok(ShadowAuthorization {
        repository: index.name.clone(),
        response: ResponseAuthorization::Repository,
    })
}

const fn require_permission(authorization: ScopedDecision) -> Result<(), ShadowRejection> {
    match authorization.decision() {
        Decision::Allow => Ok(()),
        Decision::Deny(DenyReason::NoGrant) => Err(ShadowRejection::NotFound),
        Decision::Deny(DenyReason::StorageUnavailable) => Err(ShadowRejection::Unavailable),
    }
}

#[derive(serde::Serialize)]
struct ShadowCandidateResponse {
    member: String,
    source: &'static str,
    filename: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    digest: Option<String>,
    selected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    decision: Option<ShadowDecisionResponse>,
}

/// The recorded allow, deny, or wait outcome that governs a candidate's filename. Present only when
/// policy evaluated the filename; the stored reason is already free of any upstream URL or credential.
#[derive(serde::Serialize)]
struct ShadowDecisionResponse {
    state: PolicyDecisionState,
    #[serde(skip_serializing_if = "Option::is_none")]
    rule: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    evaluated_at_unix: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_eligible_at_unix: Option<i64>,
    fresh: bool,
}

impl From<InspectedCandidate> for ShadowCandidateResponse {
    fn from(inspected: InspectedCandidate) -> Self {
        let candidate = inspected.candidate;
        let decision = if let Some(decision) = inspected.decision {
            Some(ShadowDecisionResponse {
                state: decision.state,
                rule: decision.rule,
                reason: decision.reason,
                evaluated_at_unix: decision.evaluated_at_unix,
                next_eligible_at_unix: decision.next_eligible_at_unix,
                fresh: decision.fresh,
            })
        } else {
            None
        };
        Self {
            member: candidate.member,
            source: candidate.source.as_str(),
            filename: candidate.filename,
            digest: candidate.digest,
            selected: candidate.selected,
            reason: candidate.reason.map(ShadowReason::as_str),
            decision,
        }
    }
}

fn shadow_page(page: ShadowInspection, authorization: ResponseAuthorization) -> Response {
    let candidates = page
        .candidates
        .into_iter()
        .map(ShadowCandidateResponse::from)
        .collect::<Vec<_>>();
    axum::Json(serde_json::Value::Object(
        filter_fields(
            authorization,
            [
                ClassifiedField::new(
                    "candidates",
                    FieldClassification::Repository,
                    serde_json::json!(candidates),
                ),
                ClassifiedField::new(
                    "next_cursor",
                    FieldClassification::Repository,
                    serde_json::json!(page.next_cursor),
                ),
            ],
        )
        .expect("authorization passed before the shadow query"),
    ))
    .into_response()
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"peryx-shadow\"")],
    )
        .into_response()
}

fn unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(serde_json::json!({"error": "shadow inspection service unavailable"})),
    )
        .into_response()
}

fn invalid_query() -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(serde_json::json!({"error": "invalid shadow query"})),
    )
        .into_response()
}

/// Return validation detail for client errors; redact store failures.
#[must_use]
pub fn shadow_error_response(error: &ShadowQueryError) -> Response {
    let (status, message) = match error {
        ShadowQueryError::InvalidLimit | ShadowQueryError::InvalidCursor | ShadowQueryError::ProjectTooLong => {
            (StatusCode::BAD_REQUEST, error.to_string())
        }
        ShadowQueryError::Store(_) => (StatusCode::INTERNAL_SERVER_ERROR, "shadow query failed".to_owned()),
    };
    (status, axum::Json(serde_json::json!({"error": message}))).into_response()
}
