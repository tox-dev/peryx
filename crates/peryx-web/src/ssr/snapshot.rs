use std::sync::Arc;

use axum::http::HeaderMap;
use leptos::prelude::*;
use peryx_driver::AppState;
use peryx_http::response_security::FieldClassification;

use crate::model::{UiEcosystemSummary, UiHosted, UiIndex, UiMetricFamily, UiRecentUpload, UiSnapshot, UiUpstream};

/// The dashboard snapshot, filtered to the caller's status class.
#[must_use]
pub async fn snapshot() -> UiSnapshot {
    snapshot_with_summaries(None).await
}

/// The richer admin status snapshot, filtered to the caller's status class.
#[must_use]
pub async fn admin_snapshot() -> UiSnapshot {
    snapshot_with_summaries(Some(5)).await
}

/// The class the request reads status at, resolved by the same authority as the `/+status` API so a
/// rendered page never carries a field the API would withhold.
async fn status_class(app: &AppState) -> FieldClassification {
    // `HeaderMap` extraction is infallible, so a failure defaults to no headers rather than branching.
    let headers = leptos_axum::extract::<HeaderMap>().await.unwrap_or_default();
    peryx_http::handlers::status_authorization(app, &headers)
        .await
        .field_class()
        .unwrap_or(FieldClassification::Public)
}

async fn snapshot_with_summaries(recent_limit: Option<usize>) -> UiSnapshot {
    let app = expect_context::<Arc<AppState>>();
    let class = status_class(&app).await;
    let operator = matches!(
        class,
        FieldClassification::Operator | FieldClassification::Administrator
    );
    let administrator = matches!(class, FieldClassification::Administrator);
    UiSnapshot {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        serial: if operator {
            app.meta.current_serial().unwrap_or(0)
        } else {
            0
        },
        requests: if operator {
            app.requests.load(std::sync::atomic::Ordering::Relaxed)
        } else {
            0
        },
        ecosystems: if operator { ecosystems(&app) } else { Vec::new() },
        families: if operator { families(&app) } else { Vec::new() },
        indexes: indexes(&app, administrator, recent_limit),
    }
}

fn ecosystems(app: &AppState) -> Vec<UiEcosystemSummary> {
    peryx_http::handlers::ecosystem_summaries(app)
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
        .collect()
}

fn families(app: &AppState) -> Vec<UiMetricFamily> {
    peryx_http::handlers::family_descriptors(app)
        .into_iter()
        .map(|family| UiMetricFamily {
            key: family.key,
            label: family.label,
            roles: family.roles,
        })
        .collect()
}

/// Every index's basic topology, with the administrator-class fields (upstream hosts and auth,
/// hosted upload-token state, and bounded upload summaries) present only when `administrator`.
fn indexes(app: &AppState, administrator: bool, recent_limit: Option<usize>) -> Vec<UiIndex> {
    let summaries = administrator
        .then(|| recent_limit.map(|limit| app.index_summaries(limit)))
        .flatten();
    app.describe_indexes()
        .into_iter()
        .map(|index| {
            let summary = summaries
                .as_ref()
                .and_then(|summaries| summaries.get(&index.name))
                .cloned()
                .unwrap_or_default();
            let driver = app.driver_for_name(index.ecosystem);
            let endpoint = driver.as_ref().map_or_else(
                || format!("/{}/", index.route),
                |driver| driver.client_endpoint(&index.route),
            );
            let upload = driver.and_then(|driver| {
                driver
                    .capabilities()
                    .upload_ui
                    .and_then(|driver| driver.upload_ui(&index.route, index.uploads))
            });
            UiIndex {
                name: index.name,
                route: index.route,
                ecosystem: index.ecosystem.to_owned(),
                endpoint,
                kind: index.kind.to_owned(),
                layers: index.layers,
                uploads: index.uploads,
                upload,
                upload_to: index.upload_to,
                upstream: administrator
                    .then(|| {
                        index.upstream.map(|upstream| UiUpstream {
                            url: upstream.url,
                            auth_kind: upstream.auth.to_owned(),
                            auth_redacted: (upstream.auth != "none").then(|| "<redacted>".to_owned()),
                            status: "configured".to_owned(),
                        })
                    })
                    .flatten(),
                hosted: administrator
                    .then(|| {
                        index.hosted.map(|hosted| UiHosted {
                            volatile: hosted.volatile,
                            token_configured: hosted.upload_token.configured,
                            token_redacted: hosted.upload_token.redacted.map(str::to_owned),
                        })
                    })
                    .flatten(),
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
        .collect()
}

/// The stats tree at the requested depth.
///
/// Read from the aggregator only for a caller holding operator authority, and an empty tree
/// otherwise, so the drill never names a repository to a caller the `/+stats` API would refuse.
#[must_use]
pub async fn stats(route: Option<&str>, project: Option<&str>) -> serde_json::Value {
    let app = expect_context::<Arc<AppState>>();
    if matches!(
        status_class(&app).await,
        FieldClassification::Operator | FieldClassification::Administrator
    ) {
        app.metrics.drill(route, project)
    } else {
        serde_json::json!({})
    }
}
