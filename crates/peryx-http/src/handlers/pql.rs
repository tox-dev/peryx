//! Structural scope injection and shared field classification keep primary and replica results equal.
//! Operator-classified results are never cached; the query surface has no mutation operations.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::Extension;
use axum::body::Body;
use axum::extract::State;
use axum::http::{Extensions, HeaderMap, Request, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use peryx_driver::authz::{Decision, DenyReason, ScopedDecision};
use peryx_driver::http_services::HttpDomainServices;
use peryx_driver::state::{AppState, Index};
use peryx_identity::{Action, Denial, Resource, Scope, UserId, parse_basic};
use peryx_pql::ast::{CompareOp, Predicate};
use peryx_pql::catalog::{FieldClass, FieldVisibility};
use peryx_pql::{OutputColumn, Page, PqlError, QueryScope, RepoScope, StatusClass, Value as PqlValue, bind, parse};

use crate::response_security::{
    ClassifiedField, FieldClassification, ProtectedCachePolicy, ResponseAuthorization, filter_fields,
};
use peryx_driver::route_auth::AdminRealm;

const MAX_BODY_BYTES: usize = 8 * 1024;

pub async fn pql_query(
    State(state): State<Arc<AppState>>,
    Extension(services): Extension<HttpDomainServices>,
    request: Request<Body>,
) -> Response {
    let (parts, body) = request.into_parts();
    let response = query_response(&state, &services, &parts.headers, &parts.extensions, body).await;
    apply_cache_policy(response)
}

async fn query_response(
    state: &AppState,
    services: &HttpDomainServices,
    headers: &HeaderMap,
    extensions: &Extensions,
    body: Body,
) -> Response {
    if !super::is_json(headers) {
        return problem(StatusCode::UNSUPPORTED_MEDIA_TYPE, "request body must be JSON");
    }
    let body = match read_body(body).await {
        Ok(body) => body,
        Err(error) => return error.response(),
    };
    let params = match convert_params(body.params) {
        Ok(params) => params,
        Err(error) => return error.response(),
    };
    let ast = match parse(&body.query).and_then(|ast| bind(ast, &params)) {
        Ok(ast) => ast,
        Err(error) => return pql_error(&error),
    };
    let identity = match authenticate(state, headers, extensions).await {
        Ok(identity) => identity,
        Err(rejection) => return rejection.response(),
    };
    let named = repository_equality(ast.predicate.as_ref());
    let authorization = match authorize(state, headers, named, &identity) {
        Ok(authorization) => authorization,
        Err(rejection) => return rejection.response(),
    };
    let response_authorization = authorization.response;
    let result = state
        .blocking_scans
        .run({
            let services = services.clone();
            move |cancellation| {
                services
                    .pql()
                    .execute(&ast, &authorization.scope, body.cursor.as_deref(), cancellation)
            }
        })
        .await;
    match result {
        Ok(Ok(page)) => render(&page, response_authorization),
        Ok(Err(error)) => pql_error(&error),
        Err(error) => problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("query scan worker failed: {error}"),
        ),
    }
}

struct Authorization {
    scope: QueryScope,
    response: ResponseAuthorization,
}

enum Identity {
    Local(UserId),
    EcosystemCredential,
}

#[derive(Clone, Copy)]
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
            Self::Unavailable => problem(StatusCode::SERVICE_UNAVAILABLE, "query service unavailable"),
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, AdminRealm::Query.challenge())],
            )
                .into_response(),
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

fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    named: Option<String>,
    identity: &Identity,
) -> Result<Authorization, Rejection> {
    match identity {
        Identity::Local(actor) => authorize_local(state, actor, named),
        Identity::EcosystemCredential => authorize_ecosystem(state, headers, named),
    }
}

fn authorize_local(state: &AppState, actor: &UserId, named: Option<String>) -> Result<Authorization, Rejection> {
    let Some(repository) = named else {
        let decision =
            state
                .serving
                .authorization
                .authorize_scoped(actor, Scope::AdministrationRead, &Resource::Operator);
        require_grant(decision)?;
        return Ok(Authorization {
            scope: query_scope(RepoScope::All, ResponseAuthorization::Scoped(decision), "all"),
            response: ResponseAuthorization::Scoped(decision),
        });
    };
    let index = index_by_name(state, &repository)?;
    let decision = state.serving.authorization.authorize_scoped(
        actor,
        Scope::RepositoryRead,
        &Resource::Repository(index.name.clone()),
    );
    require_grant(decision)?;
    Ok(Authorization {
        scope: repository_scope(&index.name, ResponseAuthorization::Scoped(decision)),
        response: ResponseAuthorization::Scoped(decision),
    })
}

fn authorize_ecosystem(
    state: &AppState,
    headers: &HeaderMap,
    named: Option<String>,
) -> Result<Authorization, Rejection> {
    let repository = named.ok_or(Rejection::Unauthorized)?;
    let index = state
        .serving
        .indexes
        .iter()
        .find(|index| index.name == repository)
        .ok_or(Rejection::Unauthorized)?;
    let authorization = headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok());
    match state.authorize_index_credential(index, authorization, Action::Read) {
        Ok(()) => Ok(Authorization {
            scope: repository_scope(&index.name, ResponseAuthorization::Repository),
            response: ResponseAuthorization::Repository,
        }),
        Err(Denial::Forbidden) => Err(Rejection::Forbidden),
        Err(Denial::Unavailable | Denial::Unauthenticated) => Err(Rejection::Unauthorized),
    }
}

const fn require_grant(decision: ScopedDecision) -> Result<(), Rejection> {
    match decision.decision() {
        Decision::Allow => Ok(()),
        Decision::Deny(DenyReason::NoGrant) => Err(Rejection::NotFound),
        Decision::Deny(DenyReason::StorageUnavailable) => Err(Rejection::Unavailable),
    }
}

/// PQL's `repository` column is the stored repository name - the value grants and decision records
/// carry - not the URL route, which may differ. Matching on the name keeps the injected scope, the
/// caller's `repository ==` filter, and the stored rows all speaking the same identifier.
fn index_by_name<'state>(state: &'state AppState, repository: &str) -> Result<&'state Index, Rejection> {
    state
        .serving
        .indexes
        .iter()
        .find(|index| index.name == repository)
        .ok_or(Rejection::NotFound)
}

fn repository_scope(name: &str, response: ResponseAuthorization) -> QueryScope {
    let mut set = BTreeSet::new();
    set.insert(name.to_owned());
    query_scope(RepoScope::Only(set), response, name)
}

fn query_scope(repositories: RepoScope, response: ResponseAuthorization, name: &str) -> QueryScope {
    QueryScope::new(repositories, visibility(response), fingerprint(response, name))
}

/// The evaluator hides every column the caller may not read before it plans, so a protected column
/// cannot shape a page through a filter, an order term, a group key, or an aggregate. The class set
/// comes from the same primitive that filters the response, so the two boundaries cannot disagree.
fn visibility(response: ResponseAuthorization) -> FieldVisibility {
    FieldVisibility::new(
        [
            FieldClass::Public,
            FieldClass::Repository,
            FieldClass::Operator,
            FieldClass::Administrator,
        ]
        .into_iter()
        .filter(|class| visible(*class, response)),
    )
}

/// A stable encoding of everything that decides which rows and fields the caller sees, so a cursor
/// minted under one grant cannot be replayed under another.
fn fingerprint(response: ResponseAuthorization, repositories: &str) -> String {
    let class = response.field_class().map_or("denied", classification_name);
    format!("{class}|{repositories}")
}

const fn classification_name(class: FieldClassification) -> &'static str {
    match class {
        FieldClassification::Public => "public",
        FieldClassification::Repository => "repository",
        FieldClassification::Operator => "operator",
        FieldClassification::Administrator => "administrator",
    }
}

/// Find a top-level `repository == "..."` equality so the caller's grant can be scoped to it.
fn repository_equality(predicate: Option<&Predicate>) -> Option<String> {
    match predicate? {
        Predicate::And(left, right) => repository_equality(Some(left)).or_else(|| repository_equality(Some(right))),
        Predicate::Compare {
            field,
            op: CompareOp::Eq,
            value,
        } if field == "repository" => match peryx_pql::literal_value(value) {
            PqlValue::Str(value) => Some(value),
            _ => None,
        },
        _ => None,
    }
}

fn render(page: &Page, authorization: ResponseAuthorization) -> Response {
    let rows: Vec<serde_json::Value> = page
        .rows
        .iter()
        .map(|row| render_row(&page.outputs, row, authorization))
        .collect();
    let fields = filter_fields(
        authorization,
        [
            ClassifiedField::new("rows", FieldClassification::Public, serde_json::json!(rows)),
            ClassifiedField::new(
                "next_cursor",
                FieldClassification::Public,
                serde_json::json!(page.next_cursor),
            ),
        ],
    );
    let Ok(fields) = fields else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut response = axum::Json(serde_json::Value::Object(fields)).into_response();
    let policy = if operator_visible(&page.outputs, authorization) {
        ProtectedCachePolicy::NoStore
    } else {
        ProtectedCachePolicy::Private
    };
    response.extensions_mut().insert(CachePolicy(policy));
    response
}

fn render_row(outputs: &[OutputColumn], row: &[PqlValue], authorization: ResponseAuthorization) -> serde_json::Value {
    let mut object = serde_json::Map::new();
    for (output, value) in outputs.iter().zip(row) {
        if visible(output.class, authorization) {
            object.insert(output.name.clone(), value.to_json());
        }
    }
    serde_json::Value::Object(object)
}

fn operator_visible(outputs: &[OutputColumn], authorization: ResponseAuthorization) -> bool {
    outputs.iter().any(|output| {
        matches!(output.class, FieldClass::Operator | FieldClass::Administrator) && visible(output.class, authorization)
    })
}

/// Ask the shared field-classification primitive whether a column of this class survives for the
/// caller, so the row filter and the cache decision reuse the exact rule every endpoint uses rather
/// than a parallel one.
fn visible(class: FieldClass, authorization: ResponseAuthorization) -> bool {
    let probe = ClassifiedField::new("_", classification_of(class), serde_json::Value::Null);
    filter_fields(authorization, [probe]).is_ok_and(|surviving| !surviving.is_empty())
}

const fn classification_of(class: FieldClass) -> FieldClassification {
    match class {
        FieldClass::Public => FieldClassification::Public,
        FieldClass::Repository => FieldClassification::Repository,
        FieldClass::Operator => FieldClassification::Operator,
        FieldClass::Administrator => FieldClassification::Administrator,
    }
}

#[derive(Clone, Copy)]
struct CachePolicy(ProtectedCachePolicy);

fn apply_cache_policy(mut response: Response) -> Response {
    let policy = response
        .extensions()
        .get::<CachePolicy>()
        .map_or(ProtectedCachePolicy::NoStore, |policy| policy.0);
    policy.apply(response.headers_mut());
    response
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryBody {
    query: String,
    #[serde(default)]
    params: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    cursor: Option<String>,
}

#[derive(Clone, Copy)]
enum BadRequest {
    TooLarge,
    InvalidBody,
    BadParam,
}

impl BadRequest {
    fn response(self) -> Response {
        match self {
            Self::TooLarge => problem(StatusCode::PAYLOAD_TOO_LARGE, "request body is too large"),
            Self::InvalidBody => problem(StatusCode::UNPROCESSABLE_ENTITY, "invalid request body"),
            Self::BadParam => problem(StatusCode::BAD_REQUEST, "a query parameter has an unsupported type"),
        }
    }
}

async fn read_body(body: Body) -> Result<QueryBody, BadRequest> {
    let bytes = axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| BadRequest::TooLarge)?;
    serde_json::from_slice(&bytes).map_err(|_| BadRequest::InvalidBody)
}

fn convert_params(params: BTreeMap<String, serde_json::Value>) -> Result<peryx_pql::Params, BadRequest> {
    params
        .into_iter()
        .map(|(name, value)| convert_param(value).map(|value| (name, value)))
        .collect::<Option<_>>()
        .ok_or(BadRequest::BadParam)
}

fn convert_param(value: serde_json::Value) -> Option<PqlValue> {
    match value {
        serde_json::Value::String(text) => Some(PqlValue::Str(text)),
        serde_json::Value::Bool(flag) => Some(PqlValue::Bool(flag)),
        serde_json::Value::Number(number) => number.as_i64().map(PqlValue::Int),
        _ => None,
    }
}

fn pql_error(error: &PqlError) -> Response {
    let status = match error.status() {
        StatusClass::BadRequest => StatusCode::BAD_REQUEST,
        StatusClass::NotFound => StatusCode::NOT_FOUND,
        StatusClass::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
    };
    if status == StatusCode::NOT_FOUND {
        return status.into_response();
    }
    problem(status, error.to_string())
}

fn problem(status: StatusCode, message: impl Into<String>) -> Response {
    (status, axum::Json(serde_json::json!({ "error": message.into() }))).into_response()
}

#[cfg(test)]
#[path = "../../tests/unit/handlers/pql/pure_tests.rs"]
mod pure_tests;
