//! `POST /+query`: the Peryx Query Language endpoint.
//!
//! One typed, read-only query surface over the operational domains. This handler resolves the caller
//! into a single scope, hands the query to `peryx-pql` for parsing, cost-bounded planning, structural
//! scope injection, and evaluation, then filters the result columns through the same
//! [`FieldClassification`] primitive every other endpoint uses, so a replica and the primary answer
//! identically. Operator-classified results are never cached.
//!
//! The endpoint is read-only. `peryx-pql` has no write surface, and this handler adds none: a query
//! can select, filter, order, aggregate, and page, and nothing else.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use peryx_driver::authz::{Decision, DenyReason, ScopedDecision};
use peryx_driver::state::{AppState, Index};
use peryx_events::metrics::{Metrics, PackageUsage};
use peryx_identity::{Action, Denial, Resource, Scope, UserId, authorize_all, parse_basic};
use peryx_pql::ast::{CompareOp, Literal, Predicate};
use peryx_pql::catalog::{Column, DomainAuth, DomainSchema, FieldClass, Indexability};
use peryx_pql::{
    DataSource, FetchFilter, OutputColumn, Page, PqlError, QueryScope, RepoScope, Row, StatusClass, Value as PqlValue,
    ValueType, bind, execute, parse,
};
use peryx_storage::meta::{MetaStore, PolicyDecisionItem, PolicyDecisionQuery};

use crate::response_security::{
    ClassifiedField, FieldClassification, ProtectedCachePolicy, ResponseAuthorization, filter_fields,
};

const POLICY_DOMAIN: &str = "policy.decisions";
const USAGE_DOMAIN: &str = "usage.downloads";
const MAX_BODY_BYTES: usize = 8 * 1024;
const STORE_PAGE: usize = 100;

/// `POST /+query`: run one PQL query and return typed rows.
pub async fn pql_query(State(state): State<Arc<AppState>>, request: Request<Body>) -> Response {
    let (parts, body) = request.into_parts();
    let response = query_response(&state, &parts.headers, body).await;
    apply_cache_policy(response)
}

async fn query_response(state: &AppState, headers: &HeaderMap, body: Body) -> Response {
    if !is_json(headers) {
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
    let identity = match authenticate(state, headers).await {
        Ok(identity) => identity,
        Err(rejection) => return rejection.response(),
    };
    let named = repository_equality(ast.predicate.as_ref());
    let authorization = match authorize(state, headers, named, &identity) {
        Ok(authorization) => authorization,
        Err(rejection) => return rejection.response(),
    };
    let source = NeutralSource::new(state.meta.clone(), state.metrics.clone());
    match execute(&ast, &authorization.scope, body.cursor.as_deref(), &source) {
        Ok(page) => render(&page, authorization.response),
        Err(error) => pql_error(&error),
    }
}

struct Authorization {
    scope: QueryScope,
    response: ResponseAuthorization,
}

enum Identity {
    Local(UserId),
    LegacyToken,
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
                [(header::WWW_AUTHENTICATE, "Basic realm=\"peryx-query\"")],
            )
                .into_response(),
        }
    }
}

async fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<Identity, Rejection> {
    let credentials = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_basic)
        .ok_or(Rejection::Unauthorized)?;
    if credentials.user == "__token__" {
        return Ok(Identity::LegacyToken);
    }
    state
        .users
        .authenticate(&credentials.user, &credentials.password)
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
        Identity::LegacyToken => authorize_legacy(state, headers, named),
    }
}

fn authorize_local(state: &AppState, actor: &UserId, named: Option<String>) -> Result<Authorization, Rejection> {
    let Some(repository) = named else {
        let decision = state
            .authorization
            .authorize_scoped(actor, Scope::AdministrationRead, &Resource::Operator);
        require_grant(decision)?;
        return Ok(Authorization {
            scope: QueryScope::new(
                RepoScope::All,
                fingerprint(ResponseAuthorization::Scoped(decision), "all"),
            ),
            response: ResponseAuthorization::Scoped(decision),
        });
    };
    let index = index_by_name(state, &repository)?;
    let decision =
        state
            .authorization
            .authorize_scoped(actor, Scope::RepositoryRead, &Resource::Repository(index.name.clone()));
    require_grant(decision)?;
    Ok(Authorization {
        scope: repository_scope(&index.name, ResponseAuthorization::Scoped(decision)),
        response: ResponseAuthorization::Scoped(decision),
    })
}

fn authorize_legacy(state: &AppState, headers: &HeaderMap, named: Option<String>) -> Result<Authorization, Rejection> {
    let repository = named.ok_or(Rejection::Unauthorized)?;
    let index = state
        .indexes
        .iter()
        .find(|index| index.name == repository)
        .ok_or(Rejection::Unauthorized)?;
    let authorization = headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok());
    let principal = index.acl.identify(authorization, (state.clock)()).principal;
    match authorize_all(&principal, &index.acl, Action::Read) {
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

/// Resolve the repository the query named to its configured index by the stable repository name.
///
/// PQL's `repository` column is the stored repository name - the value grants and decision records
/// carry - not the URL route, which may differ. Matching on the name keeps the injected scope, the
/// caller's `repository ==` filter, and the stored rows all speaking the same identifier.
fn index_by_name<'state>(state: &'state AppState, repository: &str) -> Result<&'state Index, Rejection> {
    state
        .indexes
        .iter()
        .find(|index| index.name == repository)
        .ok_or(Rejection::NotFound)
}

fn repository_scope(name: &str, response: ResponseAuthorization) -> QueryScope {
    let mut set = BTreeSet::new();
    set.insert(name.to_owned());
    QueryScope::new(RepoScope::Only(set), fingerprint(response, name))
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
            value: Literal::Str(value),
        } if field == "repository" => Some(value.clone()),
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

/// The read-only data source backing the neutral domains: `policy.decisions` over the decision
/// history store and `usage.downloads` over the durable usage tree. Both are ecosystem-agnostic, so
/// this source carries no per-ecosystem branch.
struct NeutralSource {
    meta: MetaStore,
    metrics: Metrics,
    policy: DomainSchema,
    usage: DomainSchema,
}

impl NeutralSource {
    fn new(meta: MetaStore, metrics: Metrics) -> Self {
        Self {
            meta,
            metrics,
            policy: policy_schema(),
            usage: usage_schema(),
        }
    }

    fn fetch_policy(&self, scope: &QueryScope, filter: Option<&FetchFilter>) -> Result<Vec<Row>, PqlError> {
        let repository = single_repository(scope);
        let project = indexed_project(filter);
        let mut cursor = None;
        let mut rows = Vec::new();
        loop {
            let query = PolicyDecisionQuery {
                repository: repository.clone(),
                project: project.clone(),
                cursor: cursor.take(),
                limit: STORE_PAGE,
                ..PolicyDecisionQuery::default()
            };
            let page = self
                .meta
                .query_policy_decisions(&query)
                .map_err(|error| PqlError::Backend(error.to_string()))?;
            rows.extend(page.decisions.iter().map(row_of));
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(rows)
    }

    fn fetch_usage(&self, scope: &QueryScope) -> Vec<Row> {
        self.metrics
            .usage_totals(single_repository(scope).as_deref())
            .into_iter()
            .map(usage_row_of)
            .collect()
    }
}

impl DataSource for NeutralSource {
    fn schema(&self, domain: &str) -> Option<&DomainSchema> {
        match domain {
            POLICY_DOMAIN => Some(&self.policy),
            USAGE_DOMAIN => Some(&self.usage),
            _ => None,
        }
    }

    fn fetch(&self, domain: &str, scope: &QueryScope, filter: Option<&FetchFilter>) -> Result<Vec<Row>, PqlError> {
        match domain {
            USAGE_DOMAIN => Ok(self.fetch_usage(scope)),
            _ => self.fetch_policy(scope, filter),
        }
    }
}

fn usage_row_of(usage: PackageUsage) -> Row {
    Row::new()
        .with("repository", PqlValue::Str(usage.repository))
        .with("project", PqlValue::Str(usage.project))
        .with("downloads", PqlValue::Int(clamp_i64(usage.downloads)))
        .with("bytes", PqlValue::Int(clamp_i64(usage.bytes)))
}

fn clamp_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn single_repository(scope: &QueryScope) -> Option<String> {
    match scope.repositories() {
        RepoScope::Only(set) if set.len() == 1 => set.iter().next().cloned(),
        RepoScope::All | RepoScope::Only(_) => None,
    }
}

/// Narrow the store read through the `project` index when the cost gate's leading filter is a single
/// project equality, so an indexed-equality predicate reaches redb as an indexed lookup instead of a
/// full page-through of the domain (constraint 6). Any other leading filter falls back to the
/// executor's in-memory predicate.
fn indexed_project(filter: Option<&FetchFilter>) -> Option<String> {
    let filter = filter?;
    match (filter.column, filter.values.as_slice()) {
        ("project", [PqlValue::Str(value)]) => Some(value.clone()),
        _ => None,
    }
}

fn row_of(item: &PolicyDecisionItem) -> Row {
    let record = &item.record;
    Row::new()
        .with("repository", PqlValue::Str(record.repository.clone()))
        .with("project", PqlValue::Str(record.project.clone()))
        .with("version", optional(record.version.as_deref()))
        .with("filename", optional(record.filename.as_deref()))
        .with("source", optional(record.source.as_deref()))
        .with("action", enum_value(&record.action))
        .with("state", enum_value(&record.state))
        .with("rule", optional(record.rule.as_deref()))
        .with("reason", optional(record.reason.as_deref()))
        .with("evaluated_at", PqlValue::Timestamp(record.evaluated_at_unix))
        .with("fresh", PqlValue::Bool(item.fresh))
}

fn optional(value: Option<&str>) -> PqlValue {
    value.map_or(PqlValue::Null, |text| PqlValue::Str(text.to_owned()))
}

fn enum_value<T: serde::Serialize>(value: &T) -> PqlValue {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(text)) => PqlValue::Str(text),
        _ => PqlValue::Null,
    }
}

const POLICY_COLUMNS: &[(&str, ValueType, FieldClass, Indexability, bool)] = &[
    (
        "repository",
        ValueType::Str,
        FieldClass::Repository,
        Indexability::Indexed,
        false,
    ),
    (
        "project",
        ValueType::Str,
        FieldClass::Repository,
        Indexability::Indexed,
        false,
    ),
    (
        "version",
        ValueType::Str,
        FieldClass::Repository,
        Indexability::Scan,
        false,
    ),
    (
        "filename",
        ValueType::Str,
        FieldClass::Repository,
        Indexability::Scan,
        false,
    ),
    (
        "source",
        ValueType::Str,
        FieldClass::Operator,
        Indexability::Scan,
        false,
    ),
    (
        "action",
        ValueType::Str,
        FieldClass::Repository,
        Indexability::Scan,
        false,
    ),
    (
        "state",
        ValueType::Str,
        FieldClass::Repository,
        Indexability::Scan,
        false,
    ),
    ("rule", ValueType::Str, FieldClass::Operator, Indexability::Scan, false),
    (
        "reason",
        ValueType::Str,
        FieldClass::Operator,
        Indexability::Scan,
        false,
    ),
    (
        "evaluated_at",
        ValueType::Timestamp,
        FieldClass::Repository,
        Indexability::KeyOrdered,
        true,
    ),
    (
        "fresh",
        ValueType::Bool,
        FieldClass::Repository,
        Indexability::Scan,
        false,
    ),
];

const USAGE_COLUMNS: &[(&str, ValueType, FieldClass, Indexability, bool)] = &[
    (
        "repository",
        ValueType::Str,
        FieldClass::Repository,
        Indexability::Indexed,
        false,
    ),
    (
        "project",
        ValueType::Str,
        FieldClass::Repository,
        Indexability::Indexed,
        false,
    ),
    (
        "downloads",
        ValueType::Int,
        FieldClass::Repository,
        Indexability::Scan,
        true,
    ),
    ("bytes", ValueType::Int, FieldClass::Operator, Indexability::Scan, true),
];

fn columns_of(declared: &[(&'static str, ValueType, FieldClass, Indexability, bool)]) -> Vec<Column> {
    declared
        .iter()
        .map(|(name, value_type, class, indexability, numeric)| {
            Column::new(name, *value_type, *class, *indexability, *numeric)
        })
        .collect()
}

fn policy_schema() -> DomainSchema {
    DomainSchema {
        name: POLICY_DOMAIN,
        columns: columns_of(POLICY_COLUMNS),
        auth: DomainAuth::RepositoryOrOperator,
        natural_order: "evaluated_at",
        bounded: true,
        // `fetch_policy` narrows the store read only on `project` (`indexed_project`); `repository` is
        // scoped by auth, not the predicate, and `evaluated_at` has no store-side seek.
        pushdown: &["project"],
    }
}

fn usage_schema() -> DomainSchema {
    DomainSchema {
        name: USAGE_DOMAIN,
        columns: columns_of(USAGE_COLUMNS),
        auth: DomainAuth::RepositoryOrOperator,
        natural_order: "downloads",
        bounded: true,
        // `fetch_usage` reads pre-aggregated totals and ignores the leading filter, so it narrows on
        // nothing.
        pushdown: &[],
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

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
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
