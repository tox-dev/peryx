use std::sync::Arc;

use leptos::prelude::*;
use peryx_driver::AppState;

use crate::model::{UiEcosystemSummary, UiHosted, UiIndex, UiMetricFamily, UiRecentUpload, UiSnapshot, UiUpstream};

/// The dashboard snapshot, read from `AppState`.
#[must_use]
pub fn snapshot() -> UiSnapshot {
    snapshot_with_summaries(None)
}

/// The richer admin status snapshot.
#[must_use]
pub fn admin_snapshot() -> UiSnapshot {
    snapshot_with_summaries(Some(5))
}

fn snapshot_with_summaries(recent_limit: Option<usize>) -> UiSnapshot {
    let app = expect_context::<Arc<AppState>>();
    let summaries = recent_limit.map(|limit| app.index_summaries(limit));
    let indexes = app
        .describe_indexes()
        .into_iter()
        .map(|index| {
            let summary = summaries
                .as_ref()
                .and_then(|summaries| summaries.get(&index.name))
                .cloned()
                .unwrap_or_default();
            UiIndex {
                name: index.name,
                route: index.route,
                ecosystem: index.ecosystem.to_owned(),
                kind: index.kind.to_owned(),
                layers: index.layers,
                uploads: index.uploads,
                upload_to: index.upload_to,
                upstream: index.upstream.map(|upstream| UiUpstream {
                    url: upstream.url,
                    auth_kind: upstream.auth.to_owned(),
                    auth_redacted: (upstream.auth != "none").then(|| "<redacted>".to_owned()),
                    status: "configured".to_owned(),
                }),
                hosted: index.hosted.map(|hosted| UiHosted {
                    volatile: hosted.volatile,
                    token_configured: hosted.upload_token.configured,
                    token_redacted: hosted.upload_token.redacted.map(str::to_owned),
                }),
                project_count: summary.project_count,
                upload_count: summary.upload_count,
                recent_uploads: summary
                    .recent_uploads
                    .into_iter()
                    .map(|upload| UiRecentUpload {
                        project: upload.project,
                        filename: upload.filename,
                        version: upload.version,
                        uploaded_at: upload.uploaded_at,
                        size: upload.size,
                    })
                    .collect(),
            }
        })
        .collect();
    UiSnapshot {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        serial: app.meta.current_serial().unwrap_or(0),
        requests: app.requests.load(std::sync::atomic::Ordering::Relaxed),
        ecosystems: peryx_http::handlers::ecosystem_summaries(&app)
            .into_iter()
            .map(|summary| UiEcosystemSummary {
                ecosystem: summary.ecosystem,
                pages: summary.pages,
                downloads: summary.downloads,
                bytes: summary.bytes,
                rejected: summary.rejected,
                uploads: summary.uploads,
                families: summary.families,
            })
            .collect(),
        families: peryx_http::handlers::family_descriptors(&app)
            .into_iter()
            .map(|family| UiMetricFamily {
                key: family.key,
                label: family.label,
                roles: family.roles,
            })
            .collect(),
        indexes,
    }
}

/// The stats tree at the requested depth, read from the metrics aggregator.
#[must_use]
pub fn stats(route: Option<&str>, project: Option<&str>) -> serde_json::Value {
    let app = expect_context::<Arc<AppState>>();
    app.metrics.drill(route, project)
}
