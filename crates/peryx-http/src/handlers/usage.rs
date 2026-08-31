use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};

use peryx_core::Role;
use peryx_driver::authz::{Decision, DenyReason};
use peryx_driver::state::{AppState, IndexDescription, IndexKind};
use peryx_identity::{Resource, Scope, parse_basic};

use crate::response_security::ProtectedCachePolicy;

/// The join lets the render layer scope role-only and ecosystem-only counters. Indexes with no
/// activity yet report zeros.
fn per_index_metrics(state: &AppState) -> Vec<(IndexDescription, peryx_events::metrics::Counters)> {
    let totals = state.serving.metrics.index_totals();
    state
        .serving
        .describe_indexes()
        .into_iter()
        .map(|index| {
            let counters = totals.get(&index.route).cloned().unwrap_or_default();
            (index, counters)
        })
        .collect()
}

/// Activity is summed across every index of an ecosystem, with that ecosystem's own counter
/// families folded in under `families`. Ordered by ecosystem name so the output is stable.
#[must_use]
pub fn ecosystem_summaries(state: &AppState) -> Vec<peryx_events::metrics::EcosystemSummary> {
    let mut summaries: std::collections::BTreeMap<String, peryx_events::metrics::EcosystemSummary> =
        std::collections::BTreeMap::new();
    for (index, counters) in per_index_metrics(state) {
        let summary =
            summaries
                .entry(index.ecosystem.clone())
                .or_insert_with(|| peryx_events::metrics::EcosystemSummary {
                    ecosystem: index.ecosystem.clone(),
                    ..Default::default()
                });
        summary.pages += counters.base.pages;
        summary.reads += counters.base.reads;
        summary.bytes += counters.base.bytes;
        summary.rejected += counters.base.rejected;
        summary.writes += counters.hosted.writes;
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
    let mut descriptors = Vec::new();
    for (ecosystem, driver) in state.driver_set().metrics() {
        for family in driver.metric_families() {
            descriptors.push(peryx_events::metrics::FamilyDescriptor {
                ecosystem: ecosystem.as_str().to_owned(),
                key: family.key.to_owned(),
                label: family.ui_label.to_owned(),
                roles: family.roles.iter().map(|role| role.as_str().to_owned()).collect(),
            });
        }
    }
    descriptors.sort_unstable_by(|left, right| {
        left.ecosystem
            .cmp(&right.ecosystem)
            .then_with(|| left.key.cmp(&right.key))
    });
    descriptors
}

/// The `/+stats` drill-down selectors.
#[derive(Debug, serde::Deserialize)]
pub struct StatsQuery {
    repository: Option<String>,
    resource: Option<String>,
}

/// The tree names repositories and resources, so it needs operator authority; a repository token
/// reads its own usage through `/+analytics/*` instead. The response is never cached.
pub async fn stats(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<StatsQuery>,
) -> Response {
    let mut response = match authorize_operator(&state, &headers).await {
        Ok(()) => {
            let mut tree = state
                .serving
                .metrics
                .drill(query.repository.as_deref(), query.resource.as_deref());
            render_extension_fields(&state, &query, &mut tree);
            axum::Json(tree).into_response()
        }
        Err(rejection) => rejection.response(),
    };
    ProtectedCachePolicy::NoStore.apply(response.headers_mut());
    response
}

/// Why a `/+stats` request was refused. An authenticated caller without an operator grant receives the
/// same `404` a missing route would, so a denial cannot confirm the endpoint to a probe.
#[derive(Debug, Clone, Copy)]
enum StatsRejection {
    NotFound,
    Unavailable,
    Unauthorized,
}

impl StatsRejection {
    fn response(self) -> Response {
        match self {
            Self::NotFound => StatusCode::NOT_FOUND.into_response(),
            Self::Unavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(serde_json::json!({"error": "stats service unavailable"})),
            )
                .into_response(),
            Self::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Basic realm=\"peryx-stats\"")],
            )
                .into_response(),
        }
    }
}

async fn authorize_operator(state: &AppState, headers: &HeaderMap) -> Result<(), StatsRejection> {
    let credentials = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_basic)
        .ok_or(StatsRejection::Unauthorized)?;
    let actor = state
        .serving
        .users
        .authenticate(&credentials.user, &credentials.password)
        .await
        .map_err(|_| StatsRejection::Unavailable)?
        .ok_or(StatsRejection::Unauthorized)?;
    match state
        .serving
        .authorization
        .authorize_scoped(&actor, Scope::OperatorRead, &Resource::Operator)
        .decision()
    {
        Decision::Allow => Ok(()),
        Decision::Deny(DenyReason::NoGrant) => Err(StatsRejection::NotFound),
        Decision::Deny(DenyReason::StorageUnavailable) => Err(StatsRejection::Unavailable),
    }
}

/// A neutral per-index counter family: name, help, the role it is scoped to (`None` = every role),
/// and the counter it reads.
struct NeutralFamily {
    name: &'static str,
    help: &'static str,
    role: Option<Role>,
    read: fn(&peryx_events::metrics::Counters) -> u64,
    kind: &'static str,
}

/// The neutral per-index families: a base group every role reports, a caching group only a cached
/// index fills, and an write group only a hosted index fills.
const NEUTRAL_FAMILIES: &[NeutralFamily] = &[
    NeutralFamily {
        name: "peryx_pages_served_total",
        help: "Index listings served.",
        role: None,
        read: |c| c.base.pages,
        kind: "counter",
    },
    NeutralFamily {
        name: "peryx_artifacts_served_total",
        help: "Artifacts served.",
        role: None,
        read: |c| c.base.reads,
        kind: "counter",
    },
    NeutralFamily {
        name: "peryx_artifacts_served_bytes_total",
        help: "Artifact bytes served.",
        role: None,
        read: |c| c.base.bytes,
        kind: "counter",
    },
    NeutralFamily {
        name: "peryx_artifacts_rejected_total",
        help: "Downloads failing digest verification.",
        role: None,
        read: |c| c.base.rejected,
        kind: "counter",
    },
    NeutralFamily {
        name: "peryx_upstream_refreshes_total",
        help: "Upstream revalidations.",
        role: Some(Role::Cached),
        read: |c| c.cached.refreshes,
        kind: "counter",
    },
    NeutralFamily {
        name: "peryx_upstream_pages_changed_total",
        help: "Revalidations that found upstream changed.",
        role: Some(Role::Cached),
        read: |c| c.cached.changed,
        kind: "counter",
    },
    NeutralFamily {
        name: "peryx_stale_pages_served_total",
        help: "Pages served stale with upstream down.",
        role: Some(Role::Cached),
        read: |c| c.cached.stale_served,
        kind: "counter",
    },
    NeutralFamily {
        name: "peryx_upstream_errors_total",
        help: "Upstream failures with nothing cached.",
        role: Some(Role::Cached),
        read: |c| c.cached.upstream_errors,
        kind: "counter",
    },
    NeutralFamily {
        name: "peryx_artifacts_uploaded_total",
        help: "Distributions uploaded.",
        role: Some(Role::Hosted),
        read: |c| c.hosted.writes,
        kind: "counter",
    },
];

/// The global request counter plus the stats tree aggregated by bounded ecosystem and role labels.
/// Role-scoped families emit only for the role that owns them; ecosystem families come from the
/// driver.
pub async fn metrics(State(state): State<Arc<AppState>>) -> Response {
    let requests = state.serving.requests.load(Ordering::Relaxed);
    let mut body = format!(
        "# HELP peryx_requests_total HTTP requests the server accepts.\n\
         # TYPE peryx_requests_total counter\n\
         peryx_requests_total {requests}\n"
    );
    write_rate_limit_metrics(&mut body, &state);
    let totals = prometheus_totals(&state);
    for family in NEUTRAL_FAMILIES {
        let _ = writeln!(body, "# HELP {} {}", family.name, family.help);
        let _ = writeln!(body, "# TYPE {} {}", family.name, family.kind);
        for ((ecosystem, role), counters) in &totals {
            if family.role.is_none_or(|family_role| family_role.as_str() == *role) {
                write_metric(&mut body, family.name, ecosystem, role, (family.read)(counters));
            }
        }
    }
    for (driver_ecosystem, driver) in state.driver_set().metrics() {
        for family in driver.metric_families() {
            let _ = writeln!(body, "# HELP {} {}", family.prom_name, family.help);
            let _ = writeln!(body, "# TYPE {} {}", family.prom_name, family.kind.as_str());
            for ((ecosystem, role), counters) in &totals {
                let mut supports_role = false;
                for family_role in family.roles {
                    if family_role.as_str() == *role {
                        supports_role = true;
                        break;
                    }
                }
                if *ecosystem == driver_ecosystem.as_str() && supports_role {
                    let values = family.json_name.map_or(&counters.ecosystem, |_| &counters.extensions);
                    let value = values.get(family.key).copied().unwrap_or(0);
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
) -> std::collections::BTreeMap<(String, &'static str), peryx_events::metrics::Counters> {
    let snapshots = state
        .serving
        .metrics
        .totals_for_routes(state.serving.indexes.iter().map(|index| index.route.as_str()));
    let mut totals = std::collections::BTreeMap::new();
    for (index, counters) in state.serving.indexes.iter().zip(snapshots) {
        let role = match &index.kind {
            IndexKind::Cached { .. } => Role::Cached,
            IndexKind::Hosted { .. } => Role::Hosted,
            IndexKind::Virtual { .. } => Role::Virtual,
        };
        merge_counters(
            totals
                .entry((index.ecosystem.as_str().to_owned(), role.as_str()))
                .or_default(),
            counters,
        );
    }
    totals
}

fn merge_counters(target: &mut peryx_events::metrics::Counters, source: peryx_events::metrics::Counters) {
    target.base.pages += source.base.pages;
    target.base.reads += source.base.reads;
    target.base.bytes += source.base.bytes;
    target.base.rejected += source.base.rejected;
    target.cached.refreshes += source.cached.refreshes;
    target.cached.changed += source.cached.changed;
    target.cached.stale_served += source.cached.stale_served;
    target.cached.upstream_errors += source.cached.upstream_errors;
    target.hosted.writes += source.hosted.writes;
    for (family, value) in source.ecosystem {
        *target.ecosystem.entry(family).or_default() += value;
    }
    for (family, value) in source.extensions {
        *target.extensions.entry(family).or_default() += value;
    }
}

fn render_extension_fields(state: &AppState, query: &StatsQuery, tree: &mut serde_json::Value) {
    let totals = state.serving.metrics.index_totals();
    let indexes: Vec<_> = state
        .serving
        .indexes
        .iter()
        .filter(|index| {
            query
                .repository
                .as_ref()
                .is_none_or(|repository| repository == &index.route)
        })
        .collect();
    for index in indexes {
        let role = match index.kind {
            IndexKind::Cached { .. } => Role::Cached,
            IndexKind::Hosted { .. } => Role::Hosted,
            IndexKind::Virtual { .. } => Role::Virtual,
        };
        let Some(metrics) = state.driver_set().get_metrics(&index.ecosystem) else {
            continue;
        };
        let fields: Vec<_> = metrics
            .metric_families()
            .iter()
            .filter(|family| family.roles.contains(&role))
            .filter_map(|family| family.json_name.map(|name| (family.key, name)))
            .collect();
        if fields.is_empty() {
            continue;
        }
        match (query.repository.as_ref(), query.resource.as_ref()) {
            (Some(_), Some(_)) => write_extension_fields(&mut tree["totals"], role, &fields, None),
            (Some(_), None) => {
                write_extension_fields(&mut tree["totals"], role, &fields, totals.get(&index.route));
                if let Some(resources) = tree["resources"].as_object_mut() {
                    for counters in resources.values_mut() {
                        write_extension_fields(counters, role, &fields, None);
                    }
                }
            }
            (None, _) => write_extension_fields(&mut tree[&index.route], role, &fields, totals.get(&index.route)),
        }
    }
}

fn write_extension_fields(
    counters: &mut serde_json::Value,
    role: Role,
    fields: &[(&str, &str)],
    totals: Option<&peryx_events::metrics::Counters>,
) {
    let Some(group) = counters
        .get_mut(role.as_str())
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    for (key, name) in fields {
        group.insert(
            (*name).to_owned(),
            serde_json::Value::from(
                totals
                    .and_then(|totals| totals.extensions.get(key))
                    .copied()
                    .unwrap_or(0),
            ),
        );
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
    for counter in state.serving.rate_limits.counters() {
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
    for counter in state.serving.rate_limits.counters() {
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
    let upstream = state.serving.upstream_limits.totals();
    let _ = writeln!(body, "peryx_upstream_rate_limit_denied_total {}", upstream.denied);
    let _ = writeln!(
        body,
        "# HELP peryx_upstream_admission_denied_total Upstream fetches refused admission before queueing.\n\
         # TYPE peryx_upstream_admission_denied_total counter"
    );
    let _ = writeln!(
        body,
        "peryx_upstream_admission_denied_total {}",
        upstream.admission_denied
    );
    let _ = writeln!(
        body,
        "# HELP peryx_upstream_inflight_fetches Current upstream fetches held by the hosted concurrency cap.\n\
         # TYPE peryx_upstream_inflight_fetches gauge"
    );
    let _ = writeln!(body, "peryx_upstream_inflight_fetches {}", upstream.in_flight);
    let _ = writeln!(
        body,
        "# HELP peryx_upstream_waiting_fetches Admitted upstream fetches waiting for a concurrency slot.\n\
         # TYPE peryx_upstream_waiting_fetches gauge"
    );
    let _ = writeln!(body, "peryx_upstream_waiting_fetches {}", upstream.waiting);
}
