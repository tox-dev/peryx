//! Usage counters: the `/+stats` drill-down, the per-ecosystem rollup, and Prometheus `/metrics`.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};

use crate::state::{AppState, IndexDescription};

/// Per-index totals joined to each index's ecosystem and role.
///
/// The join lets the render layer scope role-only and ecosystem-only counters. Indexes with no
/// activity yet report zeros.
fn per_index_metrics(state: &AppState) -> Vec<(IndexDescription, crate::metrics::Counters)> {
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
        let summary = summaries
            .entry(index.ecosystem)
            .or_insert_with(|| crate::metrics::EcosystemSummary {
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

/// `GET /+stats`: usage counters aggregated off-thread, drillable: no parameters for per-index
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
        name: "peryx_index_pages_total",
        help: "Index listings served.",
        role: None,
        read: |c| c.base.pages,
    },
    NeutralFamily {
        name: "peryx_index_downloads_total",
        help: "Artifacts served.",
        role: None,
        read: |c| c.base.downloads,
    },
    NeutralFamily {
        name: "peryx_index_download_bytes_total",
        help: "Artifact bytes served.",
        role: None,
        read: |c| c.base.bytes,
    },
    NeutralFamily {
        name: "peryx_index_rejected_total",
        help: "Downloads failing digest verification.",
        role: None,
        read: |c| c.base.rejected,
    },
    NeutralFamily {
        name: "peryx_index_refreshes_total",
        help: "Upstream revalidations.",
        role: Some("cached"),
        read: |c| c.cached.refreshes,
    },
    NeutralFamily {
        name: "peryx_index_pages_changed_total",
        help: "Revalidations that found upstream changed.",
        role: Some("cached"),
        read: |c| c.cached.changed,
    },
    NeutralFamily {
        name: "peryx_index_stale_served_total",
        help: "Pages served stale with upstream down.",
        role: Some("cached"),
        read: |c| c.cached.stale_served,
    },
    NeutralFamily {
        name: "peryx_index_upstream_errors_total",
        help: "Upstream failures with nothing cached.",
        role: Some("cached"),
        read: |c| c.cached.upstream_errors,
    },
    NeutralFamily {
        name: "peryx_index_uploads_total",
        help: "Distributions uploaded.",
        role: Some("hosted"),
        read: |c| c.hosted.uploads,
    },
];

/// `GET /metrics`: Prometheus text exposition.
///
/// The global request counter plus every per-index counter the stats tree tracks, each labelled by
/// index route, ecosystem, and role. Role-scoped families emit only for the role that owns them;
/// ecosystem families come from the driver.
pub async fn metrics(State(state): State<Arc<AppState>>) -> Response {
    let requests = state.requests.load(Ordering::Relaxed);
    let mut body = format!(
        "# HELP peryx_requests_total Total HTTP requests served.\n\
         # TYPE peryx_requests_total counter\n\
         peryx_requests_total {requests}\n"
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
fn write_metric(body: &mut String, name: &str, index: &IndexDescription, value: u64) {
    let _ = writeln!(
        body,
        "{name}{{index=\"{}\",ecosystem=\"{}\",role=\"{}\"}} {value}",
        index.route, index.ecosystem, index.kind
    );
}

fn write_rate_limit_metrics(body: &mut String, state: &AppState) {
    let _ = writeln!(
        body,
        "# HELP peryx_rate_limit_allowed_total HTTP requests allowed by the hosted rate limiter.\n\
         # TYPE peryx_rate_limit_allowed_total counter"
    );
    for counter in state.rate_limits.counters() {
        let _ = writeln!(
            body,
            "peryx_rate_limit_allowed_total{{class=\"{}\"}} {}",
            counter.class, counter.allowed
        );
    }
    let _ = writeln!(
        body,
        "# HELP peryx_rate_limit_denied_total HTTP requests denied by the hosted rate limiter.\n\
         # TYPE peryx_rate_limit_denied_total counter"
    );
    for counter in state.rate_limits.counters() {
        let _ = writeln!(
            body,
            "peryx_rate_limit_denied_total{{class=\"{}\"}} {}",
            counter.class, counter.denied
        );
    }
    let _ = writeln!(
        body,
        "# HELP peryx_upstream_rate_limit_denied_total Upstream fetches denied by the hosted concurrency cap.\n\
         # TYPE peryx_upstream_rate_limit_denied_total counter"
    );
    for counter in state.upstream_limits.snapshots() {
        let _ = writeln!(
            body,
            "peryx_upstream_rate_limit_denied_total{{index=\"{}\"}} {}",
            counter.index, counter.denied
        );
    }
    let _ = writeln!(
        body,
        "# HELP peryx_upstream_inflight_fetches Current upstream fetches held by the hosted concurrency cap.\n\
         # TYPE peryx_upstream_inflight_fetches gauge"
    );
    for counter in state.upstream_limits.snapshots() {
        let _ = writeln!(
            body,
            "peryx_upstream_inflight_fetches{{index=\"{}\"}} {}",
            counter.index, counter.in_flight
        );
    }
}
