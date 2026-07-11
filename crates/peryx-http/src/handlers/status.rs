//! `GET /+status`: health, identity, counters, and the configured indexes.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};

use super::usage::{ecosystem_summaries, family_descriptors};
use peryx_driver::state::AppState;

/// The `/+status` detail selector.
#[derive(Debug, serde::Deserialize)]
pub struct StatusQuery {
    details: Option<String>,
}

const STATUS_RECENT_UPLOADS: usize = 5;

/// `GET /+status`: health, identity, counters, and the configured indexes. The web UI's live
/// dashboard refreshes from this document.
pub async fn status(State(state): State<Arc<AppState>>, Query(query): Query<StatusQuery>) -> Response {
    let serial = state.meta.current_serial().unwrap_or(0);
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
        "serial": serial,
        "requests": state.requests.load(Ordering::Relaxed),
        "by_ecosystem": ecosystem_summaries(&state),
        "metric_families": family_descriptors(&state),
        "indexes": indexes,
    }))
    .into_response()
}
