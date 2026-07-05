//! axum request handlers.
//!
//! All index traffic arrives on a catch-all path that is resolved to a configured index by longest
//! route prefix, then handed to that index's ecosystem serving driver. The handlers here are
//! ecosystem-neutral: they dispatch to the driver and serve the cross-cutting endpoints (search,
//! status, stats, metrics, `OpenAPI`, discovery).

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::{Multipart, OriginalUri, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::search::{SearchError, SearchParams};
use crate::state::AppState;

/// The negotiated wire format for a Simple-API response.
#[derive(Clone, Copy)]
pub enum Format {
    Json,
    Html,
}

/// Pick a response format from the `Accept` header: JSON when it asks for it, HTML otherwise.
#[must_use]
pub fn negotiate(headers: &HeaderMap) -> Format {
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if accept.contains("json") {
        Format::Json
    } else {
        Format::Html
    }
}

/// `GET /{route}/...` — resolve the index's ecosystem driver and let it serve the request.
pub async fn dispatch_get(
    State(state): State<Arc<AppState>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    let serving = state.serving.clone();
    serving.get(state, uri, headers).await
}

/// `POST /{route}/` — hand the upload to the index's ecosystem driver.
pub async fn dispatch_post(
    State(state): State<Arc<AppState>>,
    Path(path): Path<String>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Response {
    let serving = state.serving.clone();
    serving.post(state, path, headers, multipart).await
}

/// `PUT /{route}/...` — hand the mutation to the index's ecosystem driver.
pub async fn dispatch_put(
    State(state): State<Arc<AppState>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    let serving = state.serving.clone();
    serving.put(state, uri, headers).await
}

/// `DELETE /{route}/...` — hand the mutation to the index's ecosystem driver.
pub async fn dispatch_delete(
    State(state): State<Arc<AppState>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    let serving = state.serving.clone();
    serving.delete(state, uri, headers).await
}

/// A `404 Not Found` with a plain body.
#[must_use]
pub fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "not found").into_response()
}

/// Run a search over cached package documents and render the result document.
#[must_use]
pub fn search_response(state: &AppState, params: SearchParams) -> Response {
    match state.search.search(state, params) {
        Ok(results) => axum::Json(results).into_response(),
        Err(err) => search_error_response(&err),
    }
}

/// Map a [`SearchError`] to a JSON error response.
#[must_use]
pub fn search_error_response(err: &SearchError) -> Response {
    let status = match err {
        SearchError::InvalidSource(_) | SearchError::Tantivy(tantivy::TantivyError::InvalidArgument(_)) => {
            StatusCode::BAD_REQUEST
        }
        SearchError::Tantivy(_)
        | SearchError::Directory(_)
        | SearchError::Io(_)
        | SearchError::Meta(_)
        | SearchError::Blob(_)
        | SearchError::Json(_)
        | SearchError::Indexer(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, axum::Json(serde_json::json!({ "error": err.to_string() }))).into_response()
}

/// `GET /api-docs/openapi.json` — the `OpenAPI` description of this server.
pub async fn openapi_spec() -> Response {
    static SPEC: std::sync::LazyLock<String> = std::sync::LazyLock::new(crate::api::openapi_json);
    ([(header::CONTENT_TYPE, "application/json")], SPEC.as_str()).into_response()
}

/// `GET /+api` — API discovery and copyable client configuration, rendered by the ecosystem driver.
pub async fn api(State(state): State<Arc<AppState>>, OriginalUri(uri): OriginalUri, headers: HeaderMap) -> Response {
    let serving = state.serving.clone();
    serving.discover(state, uri, headers).await
}

/// `GET /+search` — search cached packages across configured indexes.
pub async fn search(State(state): State<Arc<AppState>>, OriginalUri(uri): OriginalUri) -> Response {
    match SearchParams::from_query(uri.query()) {
        Ok(params) => search_response(&state, params),
        Err(err) => search_error_response(&err),
    }
}

/// The `/+status` detail selector.
#[derive(Debug, serde::Deserialize)]
pub struct StatusQuery {
    details: Option<String>,
}

const STATUS_RECENT_UPLOADS: usize = 5;

/// `GET /+status` — health, identity, counters, and the configured indexes. The web UI's live
/// dashboard refreshes from this document.
pub async fn status(State(state): State<Arc<AppState>>, Query(query): Query<StatusQuery>) -> Response {
    let serial = state.meta.current_serial().unwrap_or(0);
    let summaries = (query.details.as_deref() == Some("admin")).then(|| {
        let index_names = state.indexes.iter().map(|index| index.name.clone()).collect::<Vec<_>>();
        state
            .meta
            .summarize_indexes(&index_names, STATUS_RECENT_UPLOADS)
            .unwrap_or_default()
    });
    let indexes: Vec<serde_json::Value> = state
        .describe_indexes()
        .into_iter()
        .map(|index| {
            let mut object = serde_json::Map::from_iter([
                ("name".to_owned(), serde_json::json!(index.name)),
                ("route".to_owned(), serde_json::json!(index.route)),
                ("ecosystem".to_owned(), serde_json::json!(index.ecosystem)),
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
                    "local".to_owned(),
                    serde_json::json!(index.local.map(|local| serde_json::json!({
                        "volatile": local.volatile,
                        "upload_token": {
                            "configured": local.upload_token.configured,
                            "redacted": local.upload_token.redacted,
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
        "metadata_requests": state.metadata_requests.load(Ordering::Relaxed),
        "indexes": indexes,
    }))
    .into_response()
}

/// The `/+stats` drill-down selectors.
#[derive(Debug, serde::Deserialize)]
pub struct StatsQuery {
    index: Option<String>,
    project: Option<String>,
}

/// `GET /+stats` — usage counters aggregated off-thread, drillable: no parameters for per-index
/// totals, `?index={route}` for its projects, `&project={name}` for its files.
pub async fn stats(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<StatsQuery>,
) -> Response {
    let tree = state.metrics.drill(query.index.as_deref(), query.project.as_deref());
    axum::Json(tree).into_response()
}

/// One per-index counter family: metric name, help text, and the counter it reads.
type CounterOf = fn(&crate::metrics::Counters) -> u64;

/// `GET /metrics` — Prometheus text exposition: the two global counters plus every per-index
/// counter the stats tree tracks, labelled by index route.
pub async fn metrics(State(state): State<Arc<AppState>>) -> Response {
    let requests = state.requests.load(Ordering::Relaxed);
    let metadata = state.metadata_requests.load(Ordering::Relaxed);
    let mut body = format!(
        "# HELP velodex_requests_total Total HTTP requests served.\n\
         # TYPE velodex_requests_total counter\n\
         velodex_requests_total {requests}\n\
         # HELP velodex_metadata_requests_total PEP 658 .metadata siblings served.\n\
         # TYPE velodex_metadata_requests_total counter\n\
         velodex_metadata_requests_total {metadata}\n"
    );
    write_rate_limit_metrics(&mut body, &state);
    let mut totals: Vec<_> = state.metrics.index_totals().into_iter().collect();
    totals.sort_by(|(a, _), (b, _)| a.cmp(b));
    let families: [(&str, &str, CounterOf); 10] = [
        ("velodex_index_pages_total", "Simple pages served.", |c| c.pages),
        ("velodex_index_downloads_total", "Artifacts served.", |c| c.downloads),
        ("velodex_index_download_bytes_total", "Artifact bytes served.", |c| {
            c.bytes
        }),
        ("velodex_index_metadata_total", "PEP 658 siblings served.", |c| {
            c.metadata
        }),
        ("velodex_index_uploads_total", "Distributions uploaded.", |c| c.uploads),
        ("velodex_index_refreshes_total", "Upstream revalidations.", |c| {
            c.refreshes
        }),
        (
            "velodex_index_pages_changed_total",
            "Revalidations that found upstream changed.",
            |c| c.changed,
        ),
        (
            "velodex_index_stale_served_total",
            "Pages served stale with upstream down.",
            |c| c.stale_served,
        ),
        (
            "velodex_index_upstream_errors_total",
            "Upstream failures with nothing cached.",
            |c| c.upstream_errors,
        ),
        (
            "velodex_index_rejected_total",
            "Downloads failing digest verification.",
            |c| c.rejected,
        ),
    ];
    for (name, help, value) in families {
        let _ = writeln!(body, "# HELP {name} {help}\n# TYPE {name} counter");
        for (route, counters) in &totals {
            let _ = writeln!(body, "{name}{{index=\"{route}\"}} {}", value(counters));
        }
    }
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], body).into_response()
}

fn write_rate_limit_metrics(body: &mut String, state: &AppState) {
    let _ = writeln!(
        body,
        "# HELP velodex_rate_limit_allowed_total HTTP requests allowed by the local rate limiter.\n\
         # TYPE velodex_rate_limit_allowed_total counter"
    );
    for counter in state.rate_limits.counters() {
        let _ = writeln!(
            body,
            "velodex_rate_limit_allowed_total{{class=\"{}\"}} {}",
            counter.class, counter.allowed
        );
    }
    let _ = writeln!(
        body,
        "# HELP velodex_rate_limit_denied_total HTTP requests denied by the local rate limiter.\n\
         # TYPE velodex_rate_limit_denied_total counter"
    );
    for counter in state.rate_limits.counters() {
        let _ = writeln!(
            body,
            "velodex_rate_limit_denied_total{{class=\"{}\"}} {}",
            counter.class, counter.denied
        );
    }
    let _ = writeln!(
        body,
        "# HELP velodex_upstream_rate_limit_denied_total Upstream fetches denied by the local concurrency cap.\n\
         # TYPE velodex_upstream_rate_limit_denied_total counter"
    );
    for counter in state.upstream_limits.snapshots() {
        let _ = writeln!(
            body,
            "velodex_upstream_rate_limit_denied_total{{index=\"{}\"}} {}",
            counter.index, counter.denied
        );
    }
    let _ = writeln!(
        body,
        "# HELP velodex_upstream_inflight_fetches Current upstream fetches held by the local concurrency cap.\n\
         # TYPE velodex_upstream_inflight_fetches gauge"
    );
    for counter in state.upstream_limits.snapshots() {
        let _ = writeln!(
            body,
            "velodex_upstream_inflight_fetches{{index=\"{}\"}} {}",
            counter.index, counter.in_flight
        );
    }
}
