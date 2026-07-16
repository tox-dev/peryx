//! `GET /+status`: health, identity, counters, and the configured indexes.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::{Query, State};
use axum::http::StatusCode;
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
    let summaries = (query.details.as_deref() == Some("admin")).then(|| state.index_summaries(STATUS_RECENT_UPLOADS));
    let indexes: Vec<serde_json::Value> = state
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
                        "status": "configured",
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
        .collect();
    axum::Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "serial": serial.as_ref().copied().unwrap_or(0),
        "role": if state.read_only { "replica" } else { "writer" },
        "health": health_document(&state, serial.is_ok()),
        "requests": state.requests.load(Ordering::Relaxed),
        "by_ecosystem": ecosystem_summaries(&state),
        "metric_families": family_descriptors(&state),
        "indexes": indexes,
    }))
    .into_response()
}

/// `GET /+health`: process and local-store health for liveness checks.
pub async fn health(State(state): State<Arc<AppState>>) -> Response {
    let metadata = state.meta.current_serial().is_ok();
    let status = if metadata && blob_store_available(&state.blobs) {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, axum::Json(health_document(&state, metadata))).into_response()
}

/// `GET /+ready`: read readiness by default, or writer readiness with `?writes=true`.
pub async fn readiness(State(state): State<Arc<AppState>>, Query(query): Query<ReadinessQuery>) -> Response {
    let metadata = state.meta.current_serial().is_ok();
    let document = health_document(&state, metadata);
    let ready = document[if query.writes {
        "accepting_writes"
    } else {
        "serving_reads"
    }]
    .as_bool()
    .unwrap_or(false);
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, axum::Json(document)).into_response()
}

fn health_document(state: &AppState, metadata: bool) -> serde_json::Value {
    let blobs = blob_store_available(&state.blobs);
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

fn blob_store_available(blobs: &peryx_storage::blob::BlobStore) -> bool {
    blobs.health_check().is_ok()
}
