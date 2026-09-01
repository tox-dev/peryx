use std::sync::Arc;

use leptos::prelude::*;
use peryx_driver::AppState;
use peryx_driver::serving::{IndexSummary, IndexSummaryError};
use peryx_driver::state::IndexDescription;
use peryx_http::response_security::FieldClassification;

use crate::model::{
    UiEcosystemSummary, UiHosted, UiIndex, UiMetricFamily, UiRecentWrite, UiSnapshot, UiSummaryStatus, UiUpstream,
};

/// Returns dashboard data allowed by the caller's status authority.
#[must_use]
pub async fn snapshot() -> UiSnapshot {
    snapshot_with_summaries(None).await
}

/// Returns admin dashboard data allowed by the caller's status authority.
#[must_use]
pub async fn admin_snapshot() -> UiSnapshot {
    snapshot_with_summaries(Some(5)).await
}

async fn snapshot_with_summaries(recent_limit: Option<usize>) -> UiSnapshot {
    let app = expect_context::<Arc<AppState>>();
    let class = super::status_class(&app).await;
    app.blocking_scans
        .run({
            let app = Arc::clone(&app);
            move |_| snapshot_for_class(&app, class, recent_limit)
        })
        .await
        .expect("snapshot task never panics")
}

fn snapshot_for_class(app: &AppState, class: FieldClassification, recent_limit: Option<usize>) -> UiSnapshot {
    let operator = has_operator_access(class);
    let administrator = has_administrator_access(class);
    UiSnapshot {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        serial: operator.then(|| app.serving.meta.current_serial().ok()).flatten(),
        requests: if operator {
            app.serving.requests.load(std::sync::atomic::Ordering::Relaxed)
        } else {
            0
        },
        ecosystems: if operator { ecosystems(app) } else { Vec::new() },
        families: if operator { families(app) } else { Vec::new() },
        indexes: indexes(app, administrator, recent_limit),
    }
}

fn ecosystems(app: &AppState) -> Vec<UiEcosystemSummary> {
    peryx_http::handlers::ecosystem_summaries(app)
        .into_iter()
        .map(|summary| UiEcosystemSummary {
            ecosystem: summary.ecosystem,
            pages: summary.pages,
            reads: summary.reads,
            bytes: summary.bytes,
            rejected: summary.rejected,
            writes: summary.writes,
            families: summary.families,
        })
        .collect()
}

fn families(app: &AppState) -> Vec<UiMetricFamily> {
    peryx_http::handlers::family_descriptors(app)
        .into_iter()
        .map(|family| UiMetricFamily {
            ecosystem: family.ecosystem,
            key: family.key,
            label: family.label,
            roles: family.roles,
        })
        .collect()
}

fn indexes(app: &AppState, administrator: bool, recent_limit: Option<usize>) -> Vec<UiIndex> {
    let summaries = recent_limit
        .filter(|_| administrator)
        .map(|limit| app.index_summaries(limit));
    app.serving
        .describe_indexes()
        .into_iter()
        .map(|index| {
            let summary = summaries
                .as_ref()
                .and_then(|summaries| summaries.get(&index.name))
                .cloned();
            let endpoint = app
                .driver_for_name(&index.ecosystem)
                .and_then(|driver| app.client_discovery_for(&driver.ecosystem()))
                .map_or_else(
                    || format!("/{}/", index.route),
                    |discovery| discovery.client_endpoint(&index.route),
                );
            index_view(index, endpoint, summary, administrator)
        })
        .collect()
}

fn index_view(
    index: IndexDescription,
    endpoint: String,
    outcome: Option<Result<IndexSummary, IndexSummaryError>>,
    administrator: bool,
) -> UiIndex {
    let (summary_status, summary_error_class, summary) = match outcome {
        Some(Ok(summary)) => (UiSummaryStatus::Available, None, summary),
        Some(Err(error)) => (
            UiSummaryStatus::Unavailable,
            Some(error.as_str().to_owned()),
            IndexSummary::default(),
        ),
        None => (UiSummaryStatus::Unsupported, None, IndexSummary::default()),
    };
    UiIndex {
        name: index.name,
        route: index.route,
        ecosystem: index.ecosystem,
        endpoint,
        kind: index.kind.to_owned(),
        layers: index.layers,
        uploads: index.uploads,
        upload_to: index.upload_to,
        upstream: index.upstream.filter(|_| administrator).map(|upstream| UiUpstream {
            url: upstream.url,
            auth_kind: upstream.auth.to_owned(),
            auth_redacted: redacted_auth(upstream.auth),
            status: "configured".to_owned(),
        }),
        hosted: index.hosted.filter(|_| administrator).map(|hosted| UiHosted {
            volatile: hosted.volatile,
            token_configured: hosted.upload_token.configured,
            token_redacted: hosted.upload_token.redacted.map(str::to_owned),
        }),
        summary_status,
        summary_error_class,
        resource_count: summary.resource_count,
        write_count: summary.write_count,
        recent_writes: summary
            .recent_writes
            .into_iter()
            .map(|write| UiRecentWrite {
                resource: write.resource,
                artifact: write.artifact,
                group: write.group,
                written_at: write.written_at,
                size: write.size,
            })
            .collect(),
    }
}

const fn has_operator_access(class: FieldClassification) -> bool {
    matches!(
        class,
        FieldClassification::Operator | FieldClassification::Administrator
    )
}

const fn has_administrator_access(class: FieldClassification) -> bool {
    matches!(class, FieldClassification::Administrator)
}

fn redacted_auth(auth: &str) -> Option<String> {
    (auth != "none").then(|| "<redacted>".to_owned())
}

/// Returns metrics allowed by the caller's status authority.
#[must_use]
pub async fn stats(route: Option<&str>, resource: Option<&str>) -> serde_json::Value {
    let app = expect_context::<Arc<AppState>>();
    stats_for_class(super::status_class(&app).await, || {
        app.serving.metrics.drill(route, resource)
    })
}

fn stats_for_class(class: FieldClassification, drill: impl FnOnce() -> serde_json::Value) -> serde_json::Value {
    has_operator_access(class)
        .then(drill)
        .unwrap_or_else(|| serde_json::json!({}))
}

#[cfg(test)]
#[path = "../../tests/unit/ssr/snapshot/tests.rs"]
mod tests;
