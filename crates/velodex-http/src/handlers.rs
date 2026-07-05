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

/// Run [`search_response`] on the blocking pool. A tantivy query is mmap I/O plus CPU scoring, so
/// keeping it off the async workers stops a burst of searches from stalling concurrent serving.
///
/// # Panics
/// Panics if the blocking task panics; [`search_response`] returns every error as a response, so it
/// does not.
pub async fn search_response_offloaded(state: Arc<AppState>, params: SearchParams) -> Response {
    tokio::task::spawn_blocking(move || search_response(&state, params))
        .await
        .expect("search task never panics")
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
        Ok(params) => search_response_offloaded(state, params).await,
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

/// Per-index totals joined to each index's ecosystem and role.
///
/// The join lets the render layer scope role-only and ecosystem-only counters. Indexes with no
/// activity yet report zeros.
fn per_index_metrics(state: &AppState) -> Vec<(crate::state::IndexDescription, crate::metrics::Counters)> {
    let totals = state.metrics.index_totals();
    state
        .describe_indexes()
        .into_iter()
        .map(|index| {
            let counters = totals.get(&index.route).cloned().unwrap_or_default();
            (index, counters)
        })
        .collect()
}

/// The per-ecosystem rollup for `/+status` and the dashboard.
///
/// Activity is summed across every index of an ecosystem, with that ecosystem's own counter
/// families folded in under `families`. Ordered by ecosystem name so the output is stable.
#[must_use]
pub fn ecosystem_summaries(state: &AppState) -> Vec<crate::metrics::EcosystemSummary> {
    let mut summaries: std::collections::BTreeMap<&'static str, crate::metrics::EcosystemSummary> =
        std::collections::BTreeMap::new();
    for (index, counters) in per_index_metrics(state) {
        let summary = summaries.entry(index.ecosystem).or_insert_with(|| crate::metrics::EcosystemSummary {
            ecosystem: index.ecosystem.to_owned(),
            ..Default::default()
        });
        summary.pages += counters.base.pages;
        summary.downloads += counters.base.downloads;
        summary.bytes += counters.base.bytes;
        summary.rejected += counters.base.rejected;
        summary.uploads += counters.hosted.uploads;
        for (family, value) in counters.ecosystem {
            *summary.families.entry(family.to_owned()).or_default() += value;
        }
    }
    summaries.into_values().collect()
}

/// The driver's counter families, so the dashboard labels ecosystem counters without hardcoding any
/// ecosystem's vocabulary.
#[must_use]
pub fn family_descriptors(state: &AppState) -> Vec<crate::metrics::FamilyDescriptor> {
    state
        .serving
        .metric_families()
        .iter()
        .map(|family| crate::metrics::FamilyDescriptor {
            key: family.key.to_owned(),
            label: family.ui_label.to_owned(),
            roles: family.roles.iter().map(|role| role.as_str().to_owned()).collect(),
        })
        .collect()
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

/// A neutral per-index counter family: name, help, the role it is scoped to (`None` = every role),
/// and the counter it reads.
struct NeutralFamily {
    name: &'static str,
    help: &'static str,
    role: Option<&'static str>,
    read: fn(&crate::metrics::Counters) -> u64,
}

/// The neutral per-index families: a base group every role reports, a caching group only a cached
/// index fills, and an upload group only a hosted index fills.
const NEUTRAL_FAMILIES: &[NeutralFamily] = &[
    NeutralFamily {
        name: "velodex_index_pages_total",
        help: "Index listings served.",
        role: None,
        read: |c| c.base.pages,
    },
    NeutralFamily {
        name: "velodex_index_downloads_total",
        help: "Artifacts served.",
        role: None,
        read: |c| c.base.downloads,
    },
    NeutralFamily {
        name: "velodex_index_download_bytes_total",
        help: "Artifact bytes served.",
        role: None,
        read: |c| c.base.bytes,
    },
    NeutralFamily {
        name: "velodex_index_rejected_total",
        help: "Downloads failing digest verification.",
        role: None,
        read: |c| c.base.rejected,
    },
    NeutralFamily {
        name: "velodex_index_refreshes_total",
        help: "Upstream revalidations.",
        role: Some("cached"),
        read: |c| c.cached.refreshes,
    },
    NeutralFamily {
        name: "velodex_index_pages_changed_total",
        help: "Revalidations that found upstream changed.",
        role: Some("cached"),
        read: |c| c.cached.changed,
    },
    NeutralFamily {
        name: "velodex_index_stale_served_total",
        help: "Pages served stale with upstream down.",
        role: Some("cached"),
        read: |c| c.cached.stale_served,
    },
    NeutralFamily {
        name: "velodex_index_upstream_errors_total",
        help: "Upstream failures with nothing cached.",
        role: Some("cached"),
        read: |c| c.cached.upstream_errors,
    },
    NeutralFamily {
        name: "velodex_index_uploads_total",
        help: "Distributions uploaded.",
        role: Some("hosted"),
        read: |c| c.hosted.uploads,
    },
];

/// `GET /metrics` — Prometheus text exposition.
///
/// The global request counter plus every per-index counter the stats tree tracks, each labelled by
/// index route, ecosystem, and role. Role-scoped families emit only for the role that owns them;
/// ecosystem families come from the driver.
pub async fn metrics(State(state): State<Arc<AppState>>) -> Response {
    let requests = state.requests.load(Ordering::Relaxed);
    let mut body = format!(
        "# HELP velodex_requests_total Total HTTP requests served.\n\
         # TYPE velodex_requests_total counter\n\
         velodex_requests_total {requests}\n"
    );
    write_rate_limit_metrics(&mut body, &state);
    let mut indexes = per_index_metrics(&state);
    indexes.sort_by(|(a, _), (b, _)| a.route.cmp(&b.route));
    for family in NEUTRAL_FAMILIES {
        let _ = writeln!(body, "# HELP {} {}", family.name, family.help);
        let _ = writeln!(body, "# TYPE {} counter", family.name);
        for (index, counters) in &indexes {
            if family.role.is_none_or(|role| role == index.kind) {
                write_metric(&mut body, family.name, index, (family.read)(counters));
            }
        }
    }
    for family in state.serving.metric_families() {
        let _ = writeln!(body, "# HELP {} {}", family.prom_name, family.help);
        let _ = writeln!(body, "# TYPE {} counter", family.prom_name);
        for (index, counters) in &indexes {
            if family.roles.iter().any(|role| role.as_str() == index.kind) {
                let value = counters.ecosystem.get(family.key).copied().unwrap_or(0);
                write_metric(&mut body, family.prom_name, index, value);
            }
        }
    }
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], body).into_response()
}

/// One Prometheus sample line, labelled by index route, ecosystem, and role.
fn write_metric(body: &mut String, name: &str, index: &crate::state::IndexDescription, value: u64) {
    let _ = writeln!(
        body,
        "{name}{{index=\"{}\",ecosystem=\"{}\",role=\"{}\"}} {value}",
        index.route, index.ecosystem, index.kind
    );
}

fn write_rate_limit_metrics(body: &mut String, state: &AppState) {
    let _ = writeln!(
        body,
        "# HELP velodex_rate_limit_allowed_total HTTP requests allowed by the hosted rate limiter.\n\
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
        "# HELP velodex_rate_limit_denied_total HTTP requests denied by the hosted rate limiter.\n\
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
        "# HELP velodex_upstream_rate_limit_denied_total Upstream fetches denied by the hosted concurrency cap.\n\
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
        "# HELP velodex_upstream_inflight_fetches Current upstream fetches held by the hosted concurrency cap.\n\
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
