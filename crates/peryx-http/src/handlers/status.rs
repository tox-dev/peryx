//! Probes and routes are public. Counters require operator authority; upstream and write details require
//! administrator authority. Status responses are never shared-cached.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::Extension;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::usage::{ecosystem_summaries, family_descriptors};
use peryx_driver::access::{HeaderCredential, VerifiedCredential};
use peryx_driver::http_services::{BlobStorageStatus, HttpDomainServices};
use peryx_driver::state::AppState;
use peryx_driver::users::{AuthenticationError, PasswordDerivationError};
use peryx_identity::{Resource, Scope, UserId};

use crate::response_security::{
    ClassifiedField, FieldClassification, ProtectedCachePolicy, ResponseAuthorization, filter_fields,
};

/// Select write readiness instead of the default read readiness.
#[derive(Debug, Default, serde::Deserialize)]
pub struct ReadinessQuery {
    #[serde(default)]
    writes: bool,
}

const STATUS_RECENT_WRITES: usize = 5;

pub async fn status(
    State(state): State<Arc<AppState>>,
    Extension(services): Extension<HttpDomainServices>,
    headers: HeaderMap,
) -> Response {
    let Ok(authorization) = status_authorization(&state, &headers).await else {
        return unavailable();
    };
    let mut response = axum::Json(serde_json::Value::Object(
        status_document(&state, &services, authorization).await,
    ))
    .into_response();
    ProtectedCachePolicy::Private.apply(response.headers_mut());
    response
}

async fn status_document(
    state: &AppState,
    services: &HttpDomainServices,
    authorization: ResponseAuthorization,
) -> serde_json::Map<String, serde_json::Value> {
    let administrator = matches!(
        authorization,
        ResponseAuthorization::Scoped(decision) if matches!(decision.scope(), Scope::AdministrationRead)
    );
    let serial = services.status().current_serial();
    let blobs = services.status().blobs_healthy().await;
    let fields = [
        ClassifiedField::new(
            "version",
            FieldClassification::Public,
            serde_json::json!(env!("CARGO_PKG_VERSION")),
        ),
        ClassifiedField::new(
            "role",
            FieldClassification::Public,
            serde_json::json!(if state.serving.read_only { "replica" } else { "writer" }),
        ),
        ClassifiedField::new(
            "health",
            FieldClassification::Public,
            health_document(state, serial.is_ok(), blobs),
        ),
        ClassifiedField::new(
            "serial",
            FieldClassification::Operator,
            serde_json::json!(serial.as_ref().copied().unwrap_or(0)),
        ),
        ClassifiedField::new(
            "requests",
            FieldClassification::Operator,
            serde_json::json!(state.serving.requests.load(Ordering::Relaxed)),
        ),
        ClassifiedField::new(
            "blob_storage",
            FieldClassification::Operator,
            blob_storage_document(&services.status().blob_status()),
        ),
        ClassifiedField::new(
            "by_ecosystem",
            FieldClassification::Operator,
            serde_json::json!(ecosystem_summaries(state)),
        ),
        ClassifiedField::new(
            "metric_families",
            FieldClassification::Operator,
            serde_json::json!(family_descriptors(state)),
        ),
        ClassifiedField::new(
            "metrics_durability_failure",
            FieldClassification::Operator,
            serde_json::json!(state.serving.metrics.durability_failure()),
        ),
        ClassifiedField::new(
            "indexes",
            FieldClassification::Public,
            serde_json::Value::Array(index_documents(state, administrator)),
        ),
    ];
    filter_fields(authorization, fields).expect("public and allowed scopes classify")
}

/// # Errors
/// Returns the derivation failure when a supplied local credential cannot be checked.
pub async fn status_authorization(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<ResponseAuthorization, PasswordDerivationError> {
    let Some(actor) = status_actor(state, headers).await? else {
        return Ok(ResponseAuthorization::Public);
    };
    let administrator =
        state
            .serving
            .authorization
            .authorize_scoped(&actor, Scope::AdministrationRead, &Resource::Operator);
    if administrator.decision().is_allowed() {
        return Ok(ResponseAuthorization::Scoped(administrator));
    }
    let operator = state
        .serving
        .authorization
        .authorize_scoped(&actor, Scope::OperatorRead, &Resource::Operator);
    if operator.decision().is_allowed() {
        return Ok(ResponseAuthorization::Scoped(operator));
    }
    Ok(ResponseAuthorization::Public)
}

/// Resolves the account a status-class read speaks for.
///
/// Only a request with no `Authorization` header consults the browser session; a header the request
/// carries decides it, so a rejected credential never falls through to a cookie.
async fn status_actor(state: &AppState, headers: &HeaderMap) -> Result<Option<UserId>, PasswordDerivationError> {
    let credentials = match HeaderCredential::from_headers(&state.serving, headers) {
        HeaderCredential::Absent => {
            return Ok(peryx_driver::access::session_user(&state.serving, headers).map(|user| user.id));
        }
        HeaderCredential::Verified(VerifiedCredential::Basic(credentials)) => credentials,
        HeaderCredential::Verified(VerifiedCredential::Bearer(_)) | HeaderCredential::Invalid(_) => return Ok(None),
    };
    match state
        .serving
        .users
        .authenticate(&credentials.user, &credentials.password)
        .await
    {
        Ok(actor) => Ok(actor),
        Err(AuthenticationError::Store(_)) => Ok(None),
        Err(AuthenticationError::Derivation(error)) => Err(error),
    }
}

fn unavailable() -> Response {
    let mut response = (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(serde_json::json!({"error": "identity service unavailable"})),
    )
        .into_response();
    ProtectedCachePolicy::NoStore.apply(response.headers_mut());
    response
}

fn index_documents(state: &AppState, details: bool) -> Vec<serde_json::Value> {
    let summaries = details.then(|| state.index_summaries(STATUS_RECENT_WRITES));
    let mut documents = Vec::new();
    for index in state.serving.describe_indexes() {
        let driver = state.driver_for_name(&index.ecosystem);
        let endpoint = driver
            .and_then(|driver| state.client_discovery_for(&driver.ecosystem()))
            .map_or_else(
                || format!("/{}/", index.route),
                |discovery| discovery.client_endpoint(&index.route),
            );
        let mut object = serde_json::Map::from_iter([
            ("name".to_owned(), serde_json::json!(index.name)),
            ("route".to_owned(), serde_json::json!(index.route)),
            ("ecosystem".to_owned(), serde_json::json!(index.ecosystem)),
            ("endpoint".to_owned(), serde_json::json!(endpoint)),
            ("kind".to_owned(), serde_json::json!(index.kind)),
            ("layers".to_owned(), serde_json::json!(index.layers)),
            (
                "precedence".to_owned(),
                serde_json::json!(
                    index
                        .precedence
                        .iter()
                        .map(|member| serde_json::json!({"name": member.name, "role": member.role}))
                        .collect::<Vec<_>>()
                ),
            ),
            ("uploads".to_owned(), serde_json::json!(index.uploads)),
            ("volatile_deletes".to_owned(), serde_json::json!(index.volatile_deletes)),
            ("upload_to".to_owned(), serde_json::json!(index.upload_to)),
        ]);
        if let Some(summaries) = &summaries {
            let upstream = if let Some(upstream) = index.upstream {
                let mut sources = Vec::with_capacity(upstream.sources.len());
                for source in upstream.sources {
                    sources.push(serde_json::json!({
                        "name": source.name, "url": source.url,
                        "auth": {
                            "kind": source.auth,
                            "redacted": (source.auth != "none").then_some("<redacted>"),
                        },
                        "status": source.status,
                    }));
                }
                serde_json::json!({
                    "url": upstream.url,
                    "auth": {
                        "kind": upstream.auth,
                        "redacted": (upstream.auth != "none").then_some("<redacted>"),
                    },
                    "offline": upstream.offline,
                    "status": upstream.status,
                    "sources": sources,
                })
            } else {
                serde_json::Value::Null
            };
            object.insert("upstream".to_owned(), upstream);
            object.insert(
                "hosted".to_owned(),
                serde_json::json!(index.hosted.map(|hosted| serde_json::json!({
                    "volatile": hosted.volatile,
                    "upload_token": {
                        "configured": hosted.upload_token.configured,
                        "redacted": hosted.upload_token.redacted,
                    },
                }))),
            );
            insert_index_summary(&mut object, summaries.get(&index.name));
        }
        documents.push(serde_json::Value::Object(object));
    }
    documents
}

fn insert_index_summary(
    object: &mut serde_json::Map<String, serde_json::Value>,
    outcome: Option<&Result<peryx_driver::serving::IndexSummary, peryx_driver::serving::IndexSummaryError>>,
) {
    let summary = match outcome {
        None => {
            object.insert("summary".to_owned(), serde_json::json!({"status": "unsupported"}));
            return;
        }
        Some(Err(error)) => {
            object.insert(
                "summary".to_owned(),
                serde_json::json!({"status": "unavailable", "error_class": error.as_str()}),
            );
            return;
        }
        Some(Ok(summary)) => summary,
    };
    object.insert("summary".to_owned(), serde_json::json!({"status": "available"}));
    object.insert("resource_count".to_owned(), serde_json::json!(summary.resource_count));
    object.insert("write_count".to_owned(), serde_json::json!(summary.write_count));
    object.insert(
        "recent_writes".to_owned(),
        serde_json::json!(
            summary
                .recent_writes
                .iter()
                .map(|write| serde_json::json!({
                    "resource": write.resource,
                    "artifact": write.artifact,
                    "group": write.group,
                    "written_at": write.written_at,
                    "size": write.size,
                }))
                .collect::<Vec<_>>()
        ),
    );
}

pub async fn health() -> Response {
    probe_response(StatusCode::OK, r#"{"status":"live"}"#)
}

pub async fn readiness(State(state): State<Arc<AppState>>, Query(query): Query<ReadinessQuery>) -> Response {
    if state.serving.is_ready(query.writes).await {
        probe_response(StatusCode::OK, r#"{"status":"ready"}"#)
    } else {
        probe_response(StatusCode::SERVICE_UNAVAILABLE, r#"{"status":"not_ready"}"#)
    }
}

fn probe_response(status: StatusCode, body: &'static str) -> Response {
    (
        status,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

fn health_document(state: &AppState, metadata: bool, blobs: bool) -> serde_json::Value {
    let mut reachable = 0;
    let mut unreachable = 0;
    let mut unknown = 0;
    let mut disabled = 0;
    for index in &state.serving.indexes {
        if let peryx_driver::IndexKind::Cached { client, offline } = &index.kind {
            if *offline {
                disabled += 1;
            } else {
                let reachability = client.reachability().as_str();
                reachable += u64::from(reachability == "reachable");
                unreachable += u64::from(reachability == "unreachable");
                unknown += u64::from(reachability != "reachable" && reachability != "unreachable");
            }
        }
    }
    serde_json::json!({
        "serving_reads": metadata && blobs,
        "accepting_writes": metadata && blobs && !state.serving.read_only,
        "metadata_store": if metadata { "healthy" } else { "unhealthy" },
        "blob_store": if blobs { "healthy" } else { "unhealthy" },
        "upstreams": {
            "reachable": reachable,
            "unreachable": unreachable,
            "unknown": unknown,
            "disabled": disabled,
        },
    })
}

fn blob_storage_document(status: &BlobStorageStatus) -> serde_json::Value {
    serde_json::json!({
        "backend": status.backend,
        "capabilities": {
            "durability": status.durability,
            "conditional_write": status.conditional_write,
            "range": status.range,
            "checksum": status.checksum,
            "delete": status.delete,
            "listing": status.listing,
            "local_staging": status.local_staging,
        },
    })
}
