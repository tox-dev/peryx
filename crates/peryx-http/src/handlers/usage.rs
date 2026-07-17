//! Usage counters: the `/+stats` drill-down, the per-ecosystem rollup, and Prometheus `/metrics`.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};

use peryx_core::Role;
use peryx_driver::state::{AppState, IndexDescription, IndexKind};

/// Per-index totals joined to each index's ecosystem and role.
///
/// The join lets the render layer scope role-only and ecosystem-only counters. Indexes with no
/// activity yet report zeros.
fn per_index_metrics(state: &AppState) -> Vec<(IndexDescription, peryx_events::metrics::Counters)> {
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
pub fn ecosystem_summaries(state: &AppState) -> Vec<peryx_events::metrics::EcosystemSummary> {
    let mut summaries: std::collections::BTreeMap<&'static str, peryx_events::metrics::EcosystemSummary> =
        std::collections::BTreeMap::new();
    for (index, counters) in per_index_metrics(state) {
        let summary = summaries
            .entry(index.ecosystem)
            .or_insert_with(|| peryx_events::metrics::EcosystemSummary {
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
pub fn family_descriptors(state: &AppState) -> Vec<peryx_events::metrics::FamilyDescriptor> {
    state
        .drivers()
        .flat_map(|serving| serving.metric_families())
        .map(|family| peryx_events::metrics::FamilyDescriptor {
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

/// Pagination for the top-package usage view.
#[derive(Debug, serde::Deserialize)]
pub struct TopPackagesQuery {
    limit: Option<usize>,
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

/// `GET /+analytics/top-packages`: durable project download totals across repositories.
pub async fn top_packages(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<TopPackagesQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(25);
    if !(1..=100).contains(&limit) {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({"error": "limit must be between 1 and 100"})),
        )
            .into_response();
    }
    axum::Json(state.metrics.top_packages(limit)).into_response()
}

/// A neutral per-index counter family: name, help, the role it is scoped to (`None` = every role),
/// and the counter it reads.
struct NeutralFamily {
    name: &'static str,
    help: &'static str,
    role: Option<Role>,
    read: fn(&peryx_events::metrics::Counters) -> u64,
}

/// The neutral per-index families: a base group every role reports, a caching group only a cached
/// index fills, and an upload group only a hosted index fills.
const NEUTRAL_FAMILIES: &[NeutralFamily] = &[
    NeutralFamily {
        name: "peryx_pages_served_total",
        help: "Index listings served.",
        role: None,
        read: |c| c.base.pages,
    },
    NeutralFamily {
        name: "peryx_artifacts_served_total",
        help: "Artifacts served.",
        role: None,
        read: |c| c.base.downloads,
    },
    NeutralFamily {
        name: "peryx_artifacts_served_bytes_total",
        help: "Artifact bytes served.",
        role: None,
        read: |c| c.base.bytes,
    },
    NeutralFamily {
        name: "peryx_artifacts_rejected_total",
        help: "Downloads failing digest verification.",
        role: None,
        read: |c| c.base.rejected,
    },
    NeutralFamily {
        name: "peryx_upstream_refreshes_total",
        help: "Upstream revalidations.",
        role: Some(Role::Cached),
        read: |c| c.cached.refreshes,
    },
    NeutralFamily {
        name: "peryx_upstream_pages_changed_total",
        help: "Revalidations that found upstream changed.",
        role: Some(Role::Cached),
        read: |c| c.cached.changed,
    },
    NeutralFamily {
        name: "peryx_stale_pages_served_total",
        help: "Pages served stale with upstream down.",
        role: Some(Role::Cached),
        read: |c| c.cached.stale_served,
    },
    NeutralFamily {
        name: "peryx_upstream_errors_total",
        help: "Upstream failures with nothing cached.",
        role: Some(Role::Cached),
        read: |c| c.cached.upstream_errors,
    },
    NeutralFamily {
        name: "peryx_artifacts_uploaded_total",
        help: "Distributions uploaded.",
        role: Some(Role::Hosted),
        read: |c| c.hosted.uploads,
    },
];

/// `GET /metrics`: Prometheus text exposition.
///
/// The global request counter plus the stats tree aggregated by bounded ecosystem and role labels.
/// Role-scoped families emit only for the role that owns them; ecosystem families come from the
/// driver.
pub async fn metrics(State(state): State<Arc<AppState>>) -> Response {
    let requests = state.requests.load(Ordering::Relaxed);
    let mut body = format!(
        "# HELP peryx_requests_total Total HTTP requests served.\n\
         # TYPE peryx_requests_total counter\n\
         peryx_requests_total {requests}\n"
    );
    write_rate_limit_metrics(&mut body, &state);
    let totals = prometheus_totals(&state);
    for family in NEUTRAL_FAMILIES {
        let _ = writeln!(body, "# HELP {} {}", family.name, family.help);
        let _ = writeln!(body, "# TYPE {} counter", family.name);
        for ((ecosystem, role), counters) in &totals {
            if family.role.is_none_or(|family_role| family_role.as_str() == *role) {
                write_metric(&mut body, family.name, ecosystem, role, (family.read)(counters));
            }
        }
    }
    for driver in state.drivers() {
        for family in driver.metric_families() {
            let _ = writeln!(body, "# HELP {} {}", family.prom_name, family.help);
            let _ = writeln!(body, "# TYPE {} counter", family.prom_name);
            for ((ecosystem, role), counters) in &totals {
                if *ecosystem == driver.ecosystem().as_str()
                    && family.roles.iter().any(|family_role| family_role.as_str() == *role)
                {
                    let value = counters.ecosystem.get(family.key).copied().unwrap_or(0);
                    write_metric(&mut body, family.prom_name, ecosystem, role, value);
                }
            }
        }
    }
    state.write_process_metrics(&mut body);
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], body).into_response()
}

fn prometheus_totals(
    state: &AppState,
) -> std::collections::BTreeMap<(&'static str, &'static str), peryx_events::metrics::Counters> {
    let snapshots = state
        .metrics
        .totals_for_routes(state.indexes.iter().map(|index| index.route.as_str()));
    let mut totals = std::collections::BTreeMap::new();
    for (index, counters) in state.indexes.iter().zip(snapshots) {
        let role = match &index.kind {
            IndexKind::Cached { .. } => Role::Cached,
            IndexKind::Hosted { .. } => Role::Hosted,
            IndexKind::Virtual { .. } => Role::Virtual,
        };
        merge_counters(
            totals.entry((index.ecosystem.as_str(), role.as_str())).or_default(),
            counters,
        );
    }
    totals
}

fn merge_counters(target: &mut peryx_events::metrics::Counters, source: peryx_events::metrics::Counters) {
    target.base.pages += source.base.pages;
    target.base.downloads += source.base.downloads;
    target.base.bytes += source.base.bytes;
    target.base.rejected += source.base.rejected;
    target.cached.refreshes += source.cached.refreshes;
    target.cached.changed += source.cached.changed;
    target.cached.stale_served += source.cached.stale_served;
    target.cached.upstream_errors += source.cached.upstream_errors;
    target.hosted.uploads += source.hosted.uploads;
    for (family, value) in source.ecosystem {
        *target.ecosystem.entry(family).or_default() += value;
    }
}

/// One Prometheus sample line, labelled only by bounded ecosystem and role enums.
fn write_metric(body: &mut String, name: &str, ecosystem: &str, role: &str, value: u64) {
    let _ = writeln!(body, "{name}{{ecosystem=\"{ecosystem}\",role=\"{role}\"}} {value}");
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
    let upstream = state.upstream_limits.totals();
    let _ = writeln!(body, "peryx_upstream_rate_limit_denied_total {}", upstream.denied);
    let _ = writeln!(
        body,
        "# HELP peryx_upstream_inflight_fetches Current upstream fetches held by the hosted concurrency cap.\n\
         # TYPE peryx_upstream_inflight_fetches gauge"
    );
    let _ = writeln!(body, "peryx_upstream_inflight_fetches {}", upstream.in_flight);
}
