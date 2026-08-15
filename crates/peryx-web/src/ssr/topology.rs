use std::sync::Arc;

use axum::http::HeaderMap;
use leptos::prelude::*;
use peryx_core::{LocalStatus, NodeLiveness, TopologySnapshot, TopologyView};
use peryx_driver::AppState;
use peryx_http::response_security::FieldClassification;

#[must_use]
pub async fn topology() -> TopologySnapshot {
    let app = expect_context::<Arc<AppState>>();
    let view = topology_view(&app).await;
    let local = local_status(&app).await;
    app.serving
        .availability_topology()
        .snapshot(view, local, (app.serving.clock)())
}

async fn topology_view(app: &AppState) -> TopologyView {
    let headers = leptos_axum::extract::<HeaderMap>().await.unwrap_or_default();
    topology_view_for_class(
        peryx_http::handlers::status_authorization(app, &headers)
            .await
            .field_class(),
    )
}

const fn topology_view_for_class(class: Option<FieldClassification>) -> TopologyView {
    match class {
        Some(FieldClassification::Administrator) => TopologyView::Administrator,
        Some(FieldClassification::Operator) => TopologyView::Operator,
        _ => TopologyView::Public,
    }
}

async fn local_status(app: &AppState) -> LocalStatus {
    let serial = app.serving.meta.current_serial();
    let blobs_healthy = serial.is_ok() && app.serving.blobs.health().await.is_ok();
    local_status_from_observations(app.serving.availability_role(), serial.ok(), blobs_healthy)
}

const fn local_status_from_observations(
    role: peryx_core::NodeRole,
    serial: Option<u64>,
    blobs_healthy: bool,
) -> LocalStatus {
    LocalStatus {
        role,
        liveness: if serial.is_some() && blobs_healthy {
            NodeLiveness::Live
        } else {
            NodeLiveness::Unready
        },
        frontier: match serial {
            Some(serial) => serial,
            None => 0,
        },
    }
}

#[cfg(test)]
#[path = "../../tests/unit/ssr/topology/tests.rs"]
mod tests;
