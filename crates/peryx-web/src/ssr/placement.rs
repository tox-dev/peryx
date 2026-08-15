use std::sync::Arc;

use axum::http::HeaderMap;
use leptos::prelude::*;
use peryx_core::{
    BlobDatacenterPlacement, BlobPlacementStatus, BlobPlacementView, PlacementHealth, PlacementRow, PlacementView,
    UiArtifactSource, UiByteAvailability,
};
use peryx_driver::AppState;
use peryx_http::response_security::FieldClassification;
use peryx_identity::ArtifactDigest;
use peryx_storage::meta::{
    ArtifactPlacementHealth, ArtifactPlacementPage, ArtifactPlacementQuery, ArtifactPlacementRow, ArtifactSource,
    BlobPlacementRecord, BlobPlacementState, ByteAvailability,
};

const DEFAULT_PLACEMENT_LIMIT: usize = 25;

/// # Errors
///
/// Returns a message when the caller lacks operator access or the store cannot be read.
pub async fn placements() -> Result<PlacementView, String> {
    let app = expect_context::<Arc<AppState>>();
    let headers = leptos_axum::extract::<HeaderMap>().await.unwrap_or_default();
    let class = peryx_http::handlers::status_authorization(&app, &headers)
        .await
        .field_class();
    if !matches!(
        class,
        Some(FieldClassification::Operator | FieldClassification::Administrator)
    ) {
        return Err("You do not have access to placement health.".to_owned());
    }
    let health = app.serving.meta.artifact_placement_health().map_err(|_| ());
    let rows = if class == Some(FieldClassification::Administrator) {
        Some(
            app.serving
                .meta
                .list_artifact_placements(&ArtifactPlacementQuery {
                    cursor: None,
                    limit: DEFAULT_PLACEMENT_LIMIT,
                })
                .map_err(|_| ()),
        )
    } else {
        None
    };
    placements_for_class((app.serving.clock)(), health, rows)
}

fn placements_for_class(
    captured_at: i64,
    health: Result<ArtifactPlacementHealth, ()>,
    rows: Option<Result<ArtifactPlacementPage, ()>>,
) -> Result<PlacementView, String> {
    let Ok(health) = health else {
        return Err("Placement health could not be read.".to_owned());
    };
    let (rows, next_cursor) = if let Some(rows) = rows {
        let Ok(page) = rows else {
            return Err("Placement rows could not be read.".to_owned());
        };
        let mut projected = Vec::with_capacity(page.rows.len());
        for row in page.rows {
            projected.push(placement_row(row));
        }
        (Some(projected), page.next_cursor)
    } else {
        (None, None)
    };
    Ok(PlacementView {
        captured_at,
        health: PlacementHealth {
            local: health.local,
            remote_only: health.remote_only,
            unavailable: health.unavailable,
            total: health.total(),
        },
        rows,
        next_cursor,
    })
}

/// # Errors
///
/// Returns a message when the caller is not an administrator or the digest or store cannot be read.
pub async fn blob_placements(digest: String) -> Result<BlobPlacementView, String> {
    let app = expect_context::<Arc<AppState>>();
    let headers = leptos_axum::extract::<HeaderMap>().await.unwrap_or_default();
    let class = peryx_http::handlers::status_authorization(&app, &headers)
        .await
        .field_class();
    if class != Some(FieldClassification::Administrator) {
        return Err("You do not have access to blob placement.".to_owned());
    }
    let digest = parse_digest(&digest)?;
    let records = app.serving.meta.blob_placements(&digest).map_err(|_| ());
    blob_placements_for_digest(&digest, records)
}

fn parse_digest(digest: &str) -> Result<ArtifactDigest, String> {
    digest
        .parse()
        .map_err(|_| "That is not a valid artifact digest.".to_owned())
}

fn blob_placements_for_digest(
    digest: &ArtifactDigest,
    records: Result<Vec<BlobPlacementRecord>, ()>,
) -> Result<BlobPlacementView, String> {
    let Ok(records) = records else {
        return Err("Blob placement could not be read.".to_owned());
    };
    let mut datacenters = Vec::with_capacity(records.len());
    for record in &records {
        datacenters.push(datacenter_placement(record));
    }
    datacenters.sort_by(datacenter_order);
    Ok(BlobPlacementView {
        digest: digest.canonical(),
        datacenters,
    })
}

fn datacenter_order(left: &BlobDatacenterPlacement, right: &BlobDatacenterPlacement) -> std::cmp::Ordering {
    left.data_center
        .cmp(&right.data_center)
        .then(left.updated_at.cmp(&right.updated_at))
}

fn datacenter_placement(record: &BlobPlacementRecord) -> BlobDatacenterPlacement {
    let (status, size) = match record.state {
        BlobPlacementState::Pending => (BlobPlacementStatus::Pending, None),
        BlobPlacementState::Verified { size } => (BlobPlacementStatus::Verified, Some(size)),
        BlobPlacementState::Failed { .. } => (BlobPlacementStatus::Failed, None),
        BlobPlacementState::Revoked => (BlobPlacementStatus::Revoked, None),
    };
    BlobDatacenterPlacement {
        data_center: record.key.data_center.as_str().to_owned(),
        status,
        size,
        updated_at: record.updated_at_unix,
    }
}

fn placement_row(row: ArtifactPlacementRow) -> PlacementRow {
    PlacementRow {
        digest: row.digest,
        source: match row.source {
            ArtifactSource::Hosted => UiArtifactSource::Hosted,
            ArtifactSource::Proxy => UiArtifactSource::Proxy,
            ArtifactSource::Generated => UiArtifactSource::Generated,
        },
        availability: match row.availability {
            ByteAvailability::Local => UiByteAvailability::Local,
            ByteAvailability::RemoteOnly => UiByteAvailability::RemoteOnly,
            ByteAvailability::Unavailable => UiByteAvailability::Unavailable,
        },
    }
}

#[cfg(test)]
#[path = "../../tests/unit/ssr/placement/tests.rs"]
mod tests;
