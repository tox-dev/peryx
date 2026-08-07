//! `GET /+status`: health, identity, counters, and the configured indexes, each classified by the
//! least authority it requires.
//!
//! Version, coarse health, and the basic index list stay public, so an unauthenticated probe learns
//! liveness and the web upload and dashboard pages still resolve their routes. The operational
//! counters need operator authority; the per-index upstream hosts, upload-token state, and recent
//! uploads need administrator authority. A caller receives only the fields at or below its class, and
//! the response is never shared-cached. `GET /+health` and `GET /+ready` stay public and unfiltered.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::usage::{ecosystem_summaries, family_descriptors};
use peryx_driver::state::AppState;
use peryx_identity::{Resource, Scope, parse_basic};

use crate::response_security::{
    ClassifiedField, FieldClassification, ProtectedCachePolicy, ResponseAuthorization, filter_fields,
};

/// Select write readiness instead of the default read readiness.
#[derive(Debug, Default, serde::Deserialize)]
pub struct ReadinessQuery {
    #[serde(default)]
    writes: bool,
}

const STATUS_RECENT_UPLOADS: usize = 5;

/// `GET /+status`: health, identity, counters, and the configured indexes, filtered to the caller's
/// class. The web UI's live dashboard refreshes from this response.
pub async fn status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let authorization = status_authorization(&state, &headers).await;
    let mut response =
        axum::Json(serde_json::Value::Object(status_document(&state, authorization).await)).into_response();
    ProtectedCachePolicy::Private.apply(response.headers_mut());
    response
}

/// Build the status map already filtered to the caller's class. Every field is classified, and the
/// caller is public or an allowed scope, so the filter cannot deny.
async fn status_document(
    state: &AppState,
    authorization: ResponseAuthorization,
) -> serde_json::Map<String, serde_json::Value> {
    let administrator = matches!(
        authorization,
        ResponseAuthorization::Scoped(decision) if matches!(decision.scope(), Scope::AdministrationRead)
    );
    let serial = state.meta.current_serial();
    let blobs = state.blobs.health().await.is_ok();
    let fields = [
        ClassifiedField::new(
            "version",
            FieldClassification::Public,
            serde_json::json!(env!("CARGO_PKG_VERSION")),
        ),
        ClassifiedField::new(
            "role",
            FieldClassification::Public,
            serde_json::json!(if state.read_only { "replica" } else { "writer" }),
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
            serde_json::json!(state.requests.load(Ordering::Relaxed)),
        ),
        ClassifiedField::new(
            "blob_storage",
            FieldClassification::Operator,
            blob_storage_document(&state.blobs),
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
        // The index list carries the basic topology every caller needs to navigate and upload; its
        // sensitive per-index fields (upstream hosts, upload-token state, and recent uploads) are
        // included only for an administrator.
        ClassifiedField::new(
            "indexes",
            FieldClassification::Public,
            serde_json::Value::Array(index_documents(state, administrator)),
        ),
    ];
    filter_fields(authorization, fields).expect("public and allowed scopes classify")
}

/// The caller's status class: public for an unauthenticated, unknown, or repository-only credential;
/// operator or administrator for a local user the persisted grants raise to a server role.
///
/// The elevated checks emit their own bounded authorization events. Any authentication or grant fault
/// resolves to [`ResponseAuthorization::Public`], so a storage fault can only ever narrow what a
/// response reveals. The web renderer shares this resolver so a page reveals exactly the API's fields.
pub async fn status_authorization(state: &AppState, headers: &HeaderMap) -> ResponseAuthorization {
    let Some(credentials) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_basic)
    else {
        return ResponseAuthorization::Public;
    };
    let Ok(Some(actor)) = state.users.authenticate(&credentials.user, &credentials.password).await else {
        return ResponseAuthorization::Public;
    };
    let administrator = state
        .authorization
        .authorize_scoped(&actor, Scope::AdministrationRead, &Resource::Operator);
    if administrator.decision().is_allowed() {
        return ResponseAuthorization::Scoped(administrator);
    }
    let operator = state
        .authorization
        .authorize_scoped(&actor, Scope::OperatorRead, &Resource::Operator);
    if operator.decision().is_allowed() {
        return ResponseAuthorization::Scoped(operator);
    }
    ResponseAuthorization::Public
}

/// Describe every index. Each carries its basic topology; `details` adds the administrator-class
/// fields (upstream hosts and auth, hosted upload-token state, and bounded upload summaries).
fn index_documents(state: &AppState, details: bool) -> Vec<serde_json::Value> {
    let summaries = details.then(|| state.index_summaries(STATUS_RECENT_UPLOADS));
    state
        .describe_indexes()
        .into_iter()
        .map(|index| {
            let driver = state.driver_for_name(index.ecosystem);
            let endpoint = driver.as_ref().map_or_else(
                || format!("/{}/", index.route),
                |driver| driver.client_endpoint(&index.route),
            );
            let upload = driver.and_then(|driver| {
                driver
                    .capabilities()
                    .upload_ui
                    .and_then(|driver| driver.upload_ui(&index.route, index.uploads))
            });
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
                ("upload".to_owned(), serde_json::json!(upload)),
                ("volatile_deletes".to_owned(), serde_json::json!(index.volatile_deletes)),
                ("upload_to".to_owned(), serde_json::json!(index.upload_to)),
            ]);
            if let Some(summaries) = &summaries {
                object.insert(
                    "upstream".to_owned(),
                    serde_json::json!(index.upstream.map(|upstream| serde_json::json!({
                        "url": upstream.url,
                        "auth": {
                            "kind": upstream.auth,
                            "redacted": (upstream.auth != "none").then_some("<redacted>"),
                        },
                        "offline": upstream.offline,
                        "status": upstream.status,
                        "sources": upstream.sources.into_iter().map(|source| serde_json::json!({
                            "name": source.name, "url": source.url,
                            "auth": {
                                "kind": source.auth,
                                "redacted": (source.auth != "none").then_some("<redacted>"),
                            },
                            "status": source.status,
                        })).collect::<Vec<_>>(),
                    }))),
                );
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
                let summary = summaries.get(&index.name).cloned().unwrap_or_default();
                object.insert("project_count".to_owned(), serde_json::json!(summary.project_count));
                object.insert("upload_count".to_owned(), serde_json::json!(summary.upload_count));
                object.insert(
                    "recent_uploads".to_owned(),
                    serde_json::json!(
                        summary
                            .recent_uploads
                            .into_iter()
                            .map(|upload| {
                                serde_json::json!({
                                    "project": upload.project,
                                    "filename": upload.filename,
                                    "version": upload.version,
                                    "uploaded_at": upload.uploaded_at,
                                    "size": upload.size,
                                })
                            })
                            .collect::<Vec<_>>()
                    ),
                );
            }
            serde_json::Value::Object(object)
        })
        .collect()
}

/// `GET /+health`: process liveness for restart decisions.
pub async fn health() -> Response {
    probe_response(StatusCode::OK, r#"{"status":"live"}"#)
}

/// `GET /+ready`: read readiness by default, or writer readiness with `?writes=true`.
pub async fn readiness(State(state): State<Arc<AppState>>, Query(query): Query<ReadinessQuery>) -> Response {
    if state.is_ready(query.writes).await {
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
    for index in &state.indexes {
        if let peryx_driver::IndexKind::Cached { client, offline } = &index.kind {
            if *offline {
                disabled += 1;
            } else {
                match client.reachability().as_str() {
                    "reachable" => reachable += 1,
                    "unreachable" => unreachable += 1,
                    _ => unknown += 1,
                }
            }
        }
    }
    serde_json::json!({
        "serving_reads": metadata && blobs,
        "accepting_writes": metadata && blobs && !state.read_only,
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

fn blob_storage_document(blobs: &peryx_storage::blob::BlobStorage) -> serde_json::Value {
    let capabilities = blobs.capabilities();
    serde_json::json!({
        "backend": blobs.name(),
        "capabilities": {
            "durability": capabilities.durability.as_str(),
            "conditional_write": capabilities.create_if_absent.as_str(),
            "range": capabilities.range.as_str(),
            "checksum": capabilities.checksum.as_str(),
            "delete": capabilities.delete.as_str(),
            "listing": capabilities.list.as_str(),
            "local_staging": capabilities.local_tail.as_str(),
        },
    })
}
