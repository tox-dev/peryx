//! `GET /+status`: health, identity, counters, and the configured indexes.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::{Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

use super::usage::{ecosystem_summaries, family_descriptors};
use peryx_driver::state::AppState;

/// The `/+status` detail selector.
#[derive(Debug, serde::Deserialize)]
pub struct StatusQuery {
    details: Option<String>,
}

/// Select write readiness instead of the default read readiness.
#[derive(Debug, Default, serde::Deserialize)]
pub struct ReadinessQuery {
    #[serde(default)]
    writes: bool,
}

const STATUS_RECENT_UPLOADS: usize = 5;

/// `GET /+status`: health, identity, counters, and the configured indexes. The web UI's live
/// dashboard refreshes from this document.
pub async fn status(State(state): State<Arc<AppState>>, Query(query): Query<StatusQuery>) -> Response {
    let serial = state.meta.current_serial();
    let blobs = state.blobs.health().await.is_ok();
    let indexes = index_documents(&state, query.details.as_deref() == Some("admin"));
    axum::Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "serial": serial.as_ref().copied().unwrap_or(0),
        "role": if state.read_only { "replica" } else { "writer" },
        "health": health_document(&state, serial.is_ok(), blobs),
        "blob_storage": blob_storage_document(&state.blobs),
        "requests": state.requests.load(Ordering::Relaxed),
        "by_ecosystem": ecosystem_summaries(&state),
        "metric_families": family_descriptors(&state),
        "indexes": indexes,
    }))
    .into_response()
}

fn index_documents(state: &AppState, details: bool) -> Vec<serde_json::Value> {
    let summaries = details.then(|| state.index_summaries(STATUS_RECENT_UPLOADS));
    state
        .describe_indexes()
        .into_iter()
        .map(|index| {
            let endpoint = state.driver_for_name(index.ecosystem).map_or_else(
                || format!("/{}/", index.route),
                |driver| driver.client_endpoint(&index.route),
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
                (
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
                ),
                (
                    "hosted".to_owned(),
                    serde_json::json!(index.hosted.map(|hosted| serde_json::json!({
                        "volatile": hosted.volatile,
                        "upload_token": {
                            "configured": hosted.upload_token.configured,
                            "redacted": hosted.upload_token.redacted,
                        },
                    }))),
                ),
            ]);
            if let Some(summaries) = &summaries {
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
