use std::str::FromStr as _;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{Extensions, HeaderMap, HeaderValue, Request, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use peryx_driver::authz::{Decision, ScopedDecision};
use peryx_driver::revocations::{
    DigestRevocation, DigestRevocationQuery, DigestRevocationQueryError, DigestRevocationStatus, LiftRevocationOutcome,
    PutRevocationError, PutRevocationOutcome,
};
use peryx_driver::state::AppState;
use peryx_identity::{ArtifactDigest, Resource, RevocationReason, Scope, UserId, parse_basic};

use crate::response_security::{
    ClassifiedField, FieldClassification, ProtectedCachePolicy, ResponseAuthorization, filter_fields,
};
use peryx_driver::route_auth::AdminRealm;

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PutRevocationBody {
    reason: String,
}

#[derive(Debug, serde::Deserialize)]
pub struct DigestRevocationsQuery {
    status: Option<DigestRevocationStatus>,
    cursor: Option<String>,
    limit: Option<usize>,
}

pub async fn put_revocation(
    State(state): State<Arc<AppState>>,
    Path(digest): Path<String>,
    request: Request<Body>,
) -> Response {
    let headers = request.headers();
    let (actor, authorization) =
        match administrator(&state, headers, request.extensions(), Scope::AdministrationWrite).await {
            Ok(authenticated) => authenticated,
            Err(response) => return response,
        };
    let Ok(digest) = ArtifactDigest::from_str(&digest) else {
        return problem(StatusCode::BAD_REQUEST, "invalid digest");
    };
    if !super::is_json(headers) {
        return problem(StatusCode::UNSUPPORTED_MEDIA_TYPE, "request body must be JSON");
    }
    let body = match axum::body::to_bytes(request.into_body(), 3 * 1024).await {
        Ok(body) => match serde_json::from_slice::<PutRevocationBody>(&body) {
            Ok(body) => body,
            Err(_) => return problem(StatusCode::UNPROCESSABLE_ENTITY, "invalid request body"),
        },
        Err(_) => return problem(StatusCode::PAYLOAD_TOO_LARGE, "request body is too large"),
    };
    let Ok(reason) = RevocationReason::new(body.reason) else {
        return problem(StatusCode::BAD_REQUEST, "invalid revocation reason");
    };
    match state.serving.put_digest_revocation(&digest, &reason, &actor) {
        Ok(outcome) => {
            let status = if matches!(outcome, PutRevocationOutcome::Unchanged(_)) {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            };
            record_response(status, outcome.record(), authorization, Some(&digest))
        }
        Err(PutRevocationError::ReasonConflict) => problem(StatusCode::CONFLICT, "digest is already revoked"),
        Err(PutRevocationError::Store(_)) => unavailable(),
    }
}

pub async fn inspect_revocation(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    extensions: Extensions,
    Path(digest): Path<String>,
) -> Response {
    let (_actor, authorization) = match administrator(&state, &headers, &extensions, Scope::AdministrationRead).await {
        Ok(authenticated) => authenticated,
        Err(response) => return response,
    };
    let Ok(digest) = ArtifactDigest::from_str(&digest) else {
        return problem(StatusCode::BAD_REQUEST, "invalid digest");
    };
    match state.serving.revocations.inspect(&digest) {
        Ok(Some(record)) => record_response(StatusCode::OK, &record, authorization, None),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => unavailable(),
    }
}

pub async fn list_revocations(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    extensions: Extensions,
    Query(query): Query<DigestRevocationsQuery>,
) -> Response {
    let (_actor, _authorization) = match administrator(&state, &headers, &extensions, Scope::AdministrationRead).await {
        Ok(authenticated) => authenticated,
        Err(response) => return response,
    };
    let cursor = match query.cursor {
        Some(cursor) => match ArtifactDigest::from_str(&cursor) {
            Ok(cursor) => Some(cursor),
            Err(_) => return problem(StatusCode::BAD_REQUEST, "invalid revocation cursor"),
        },
        None => None,
    };
    match state.serving.revocations.list(&DigestRevocationQuery {
        status: query.status,
        cursor,
        limit: query.limit.unwrap_or(25),
    }) {
        Ok(page) => {
            let mut response = axum::Json(page).into_response();
            ProtectedCachePolicy::NoStore.apply(response.headers_mut());
            response
        }
        Err(DigestRevocationQueryError::InvalidLimit) => problem(StatusCode::BAD_REQUEST, "invalid limit"),
        Err(DigestRevocationQueryError::Store(_)) => unavailable(),
    }
}

pub async fn lift_revocation(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    extensions: Extensions,
    Path(digest): Path<String>,
) -> Response {
    let (actor, authorization) = match administrator(&state, &headers, &extensions, Scope::AdministrationWrite).await {
        Ok(authenticated) => authenticated,
        Err(response) => return response,
    };
    let Ok(digest) = ArtifactDigest::from_str(&digest) else {
        return problem(StatusCode::BAD_REQUEST, "invalid digest");
    };
    match state.serving.lift_digest_revocation(&digest, &actor) {
        Ok(Some(LiftRevocationOutcome::Lifted(record) | LiftRevocationOutcome::Unchanged(record))) => {
            record_response(StatusCode::OK, &record, authorization, None)
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(_) => unavailable(),
    }
}

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
        return Err(StatusCode::NOT_FOUND.into_response());
    }
    Ok((actor, decision))
}

fn record_response(
    status: StatusCode,
    record: &DigestRevocation,
    authorization: ScopedDecision,
    location: Option<&ArtifactDigest>,
) -> Response {
    let fields = [
        ClassifiedField::new(
            "digest",
            FieldClassification::Operator,
            serde_json::json!(&record.digest),
        ),
        ClassifiedField::new(
            "reason",
            FieldClassification::Administrator,
            serde_json::json!(&record.reason),
        ),
        ClassifiedField::new(
            "created_by",
            FieldClassification::Administrator,
            serde_json::json!(&record.created_by),
        ),
        ClassifiedField::new(
            "created_at_unix",
            FieldClassification::Administrator,
            serde_json::json!(record.created_at_unix),
        ),
        ClassifiedField::new("state", FieldClassification::Operator, serde_json::json!(&record.state)),
        ClassifiedField::new(
            "revision",
            FieldClassification::Operator,
            serde_json::json!(record.revision),
        ),
    ];
    let fields = filter_fields(ResponseAuthorization::Scoped(authorization), fields)
        .expect("administrator scope permits every revocation field");
    let mut response = (status, axum::Json(fields)).into_response();
    ProtectedCachePolicy::NoStore.apply(response.headers_mut());
    if let Some(digest) = location {
        response.headers_mut().insert(
            header::LOCATION,
            HeaderValue::from_str(&format!("/+revocations/{digest}")).expect("canonical digest is a valid location"),
        );
    }
    response
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, AdminRealm::Server.challenge())],
    )
        .into_response()
}

fn unavailable() -> Response {
    problem(StatusCode::SERVICE_UNAVAILABLE, "revocation service unavailable")
}

fn problem(status: StatusCode, message: &'static str) -> Response {
    (status, axum::Json(serde_json::json!({"error": message}))).into_response()
}
