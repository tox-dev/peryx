//! Responses use persisted counters and private caching to prevent cross-user reuse.

use std::sync::Arc;

use axum::Extension;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{Extensions, HeaderMap, Request, StatusCode, Uri, header};
use axum::response::{IntoResponse as _, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::json;

use peryx_driver::authz::{Decision, DenyReason, ScopedDecision};
use peryx_driver::http_services::HttpDomainServices;
use peryx_driver::state::{AppState, Index};
use peryx_identity::{Action, Resource, Scope, UserId, parse_basic};

use crate::response_security::ProtectedCachePolicy;

const DEFAULT_LIMIT: usize = 25;
const MAX_LIMIT: usize = 100;
const MAX_REPOSITORY_BYTES: usize = 512;

pub async fn quota_summary(
    State(state): State<Arc<AppState>>,
    Extension(services): Extension<HttpDomainServices>,
    request: Request<Body>,
) -> Response {
    let (parts, _) = request.into_parts();
    let mut response = summary_response(&state, &services, &parts.headers, &parts.extensions, &parts.uri).await;
    ProtectedCachePolicy::Private.apply(response.headers_mut());
    response
}

pub async fn quota_repository(
    State(state): State<Arc<AppState>>,
    Extension(services): Extension<HttpDomainServices>,
    request: Request<Body>,
) -> Response {
    let (parts, _) = request.into_parts();
    let mut response = detail_response(&state, &services, &parts.headers, &parts.extensions, &parts.uri).await;
    ProtectedCachePolicy::Private.apply(response.headers_mut());
    response
}

/// The selectors shared by both reads: the summary uses `cursor` and `limit`, the detail uses
/// `repository`. One struct keeps a malformed `limit` a parse error on either path.
#[derive(Debug, serde::Deserialize)]
struct QuotaParams {
    repository: Option<String>,
    cursor: Option<String>,
    limit: Option<usize>,
}

async fn summary_response(
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
    let Identity::Local(actor) = identity else {
        return Rejection::Forbidden.response();
    };
    if let Err(rejection) = require_administrator(state, &actor) {
        return rejection.response();
    }
    let Ok(Query(params)) = Query::<QuotaParams>::try_from_uri(uri) else {
        return bad_request("invalid quota query");
    };
    let (offset, limit) = match page_bounds(params.limit, params.cursor.as_deref()) {
        Ok(bounds) => bounds,
        Err(message) => return bad_request(message),
    };
    let Ok(mut rows) = services.quota().summaries(&state.serving.indexes, offset, limit + 1) else {
        return unavailable();
    };
    let next_cursor = (rows.len() > limit).then(|| encode_cursor(offset + limit));
    rows.truncate(limit);
    axum::Json(json!({ "repositories": rows, "next_cursor": next_cursor })).into_response()
}

async fn detail_response(
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
    let Ok(Query(params)) = Query::<QuotaParams>::try_from_uri(uri) else {
        return bad_request("invalid quota query");
    };
    let Some(route) = params.repository.as_deref() else {
        return bad_request("repository is required");
    };
    if route.len() > MAX_REPOSITORY_BYTES {
        return bad_request("repository filter exceeds 512 bytes");
    }
    let index = match authorize_repository(state, headers, route, &identity) {
        Ok(index) => index,
        Err(rejection) => return rejection.response(),
    };
    services
        .quota()
        .repository(index)
        .map_or_else(|_| unavailable(), |quota| axum::Json(quota).into_response())
}

fn page_bounds(limit: Option<usize>, cursor: Option<&str>) -> Result<(usize, usize), &'static str> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err("limit must be between 1 and 100");
    }
    let offset = match cursor {
        Some(cursor) => decode_cursor(cursor).ok_or("invalid quota cursor")?,
        None => 0,
    };
    Ok((offset, limit))
}

#[derive(Debug)]
enum Identity {
    Local(UserId),
    EcosystemCredential,
}

#[derive(Debug, Clone, Copy)]
enum Rejection {
    Forbidden,
    NotFound,
    Unavailable,
    Unauthorized,
}

impl Rejection {
    fn response(self) -> Response {
        match self {
            Self::Forbidden => StatusCode::FORBIDDEN.into_response(),
            Self::NotFound => StatusCode::NOT_FOUND.into_response(),
            Self::Unavailable => unavailable(),
            Self::Unauthorized => unauthorized(),
        }
    }
}

impl From<super::EcosystemCredentialDenied> for Rejection {
    fn from(denied: super::EcosystemCredentialDenied) -> Self {
        match denied {
            super::EcosystemCredentialDenied::Forbidden => Self::Forbidden,
            super::EcosystemCredentialDenied::Unauthorized => Self::Unauthorized,
        }
    }
}

async fn authenticate(state: &AppState, headers: &HeaderMap, extensions: &Extensions) -> Result<Identity, Rejection> {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or(Rejection::Unauthorized)?;
    if state.recognizes_index_credential(authorization) {
        return Ok(Identity::EcosystemCredential);
    }
    let credentials = parse_basic(authorization).ok_or(Rejection::Unauthorized)?;
    state
        .serving
        .users
        .authenticate_request(extensions, &credentials)
        .await
        .map_err(|_| Rejection::Unavailable)?
        .map(Identity::Local)
        .ok_or(Rejection::Unauthorized)
}

fn require_administrator(state: &AppState, actor: &UserId) -> Result<(), Rejection> {
    require_permission(state.serving.authorization.authorize_scoped(
        actor,
        Scope::AdministrationRead,
        &Resource::Operator,
    ))
}

fn authorize_repository<'state>(
    state: &'state AppState,
    headers: &HeaderMap,
    route: &str,
    identity: &Identity,
) -> Result<&'state Index, Rejection> {
    match identity {
        Identity::Local(actor) => authorize_local(state, actor, route),
        Identity::EcosystemCredential => authorize_ecosystem(state, headers, route),
    }
}

fn authorize_local<'state>(state: &'state AppState, actor: &UserId, route: &str) -> Result<&'state Index, Rejection> {
    let index = super::index_by_route(state, route).ok_or(Rejection::NotFound)?;
    let decision = state.serving.authorization.authorize_scoped(
        actor,
        Scope::RepositoryRead,
        &Resource::Repository(index.name.clone()),
    );
    require_permission(decision)?;
    Ok(index)
}

fn authorize_ecosystem<'state>(
    state: &'state AppState,
    headers: &HeaderMap,
    route: &str,
) -> Result<&'state Index, Rejection> {
    super::authorize_ecosystem_credential(state, headers, route, Action::Read).map_err(Into::into)
}

const fn require_permission(decision: ScopedDecision) -> Result<(), Rejection> {
    match decision.decision() {
        Decision::Allow => Ok(()),
        Decision::Deny(DenyReason::NoGrant) => Err(Rejection::NotFound),
        Decision::Deny(DenyReason::StorageUnavailable) => Err(Rejection::Unavailable),
    }
}

fn encode_cursor(offset: usize) -> String {
    URL_SAFE_NO_PAD.encode(offset.to_string())
}

fn decode_cursor(cursor: &str) -> Option<usize> {
    let bytes = URL_SAFE_NO_PAD.decode(cursor).ok()?;
    std::str::from_utf8(&bytes).ok()?.parse().ok()
}

fn bad_request(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, axum::Json(json!({ "error": message }))).into_response()
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Basic realm=\"peryx-quota\"")],
    )
        .into_response()
}

fn unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(json!({ "error": "quota service unavailable" })),
    )
        .into_response()
}
