use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse as _, Response};
use peryx_core::{
    BlobDatacenterPlacement, BlobPlacementStatus, BlobPlacementView, PlacementHealth, PlacementRow, PlacementView,
    UiArtifactSource, UiByteAvailability,
};
use peryx_driver::state::{AppState, ServingState};
use peryx_ha::{
    ArtifactPlacementQuery, ArtifactPlacementRow, ArtifactSource, AvailabilityAudience, BlobPlacementRecord,
    BlobPlacementState, ByteAvailability,
};
use peryx_identity::ArtifactDigest;
use peryx_storage::meta::ArtifactPlacementQueryError;

use peryx_http::response_security::ProtectedCachePolicy;

use crate::availability_http::availability_audience;

const DEFAULT_PLACEMENT_LIMIT: usize = 25;

#[derive(Debug, serde::Deserialize)]
struct PlacementsQuery {
    cursor: Option<String>,
    limit: Option<usize>,
}

pub async fn placements(State(state): State<Arc<AppState>>, headers: HeaderMap, uri: Uri) -> Response {
    let audience = availability_audience(state.serving.clone(), &headers).await;
    let mut response = placements_response(&state.serving, audience, &uri);
    ProtectedCachePolicy::NoStore.apply(response.headers_mut());
    response
}

fn placements_response(state: &ServingState, audience: AvailabilityAudience, uri: &Uri) -> Response {
    if audience == AvailabilityAudience::Public {
        return StatusCode::FORBIDDEN.into_response();
    }
    let mut rows = None;
    let mut next_cursor = None;
    if audience == AvailabilityAudience::Administrator {
        let Ok(Query(query)) = Query::<PlacementsQuery>::try_from_uri(uri) else {
            return bad_request("invalid placement query");
        };
        let query = ArtifactPlacementQuery {
            cursor: query.cursor,
            limit: query.limit.unwrap_or(DEFAULT_PLACEMENT_LIMIT),
        };
        match state.meta.list_artifact_placements(&query) {
            Ok(page) => {
                rows = Some(page.rows.into_iter().map(placement_row).collect());
                next_cursor = page.next_cursor;
            }
            Err(ArtifactPlacementQueryError::InvalidLimit) => return bad_request("placement limit out of range"),
            Err(ArtifactPlacementQueryError::Store(_)) => return internal_error(),
        }
    }
    let Ok(health) = state.meta.artifact_placement_health() else {
        return internal_error();
    };
    axum::Json(PlacementView {
        captured_at: (state.clock)(),
        health: PlacementHealth {
            local: health.local,
            remote_only: health.remote_only,
            unavailable: health.unavailable,
            total: health.total(),
        },
        rows,
        next_cursor,
    })
    .into_response()
}

fn placement_row(row: ArtifactPlacementRow) -> PlacementRow {
    PlacementRow {
        digest: row.digest,
        source: source_class(row.source),
        availability: availability_class(row.availability),
    }
}

pub async fn blob_placements(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(digest): Path<String>,
) -> Response {
    let audience = availability_audience(state.serving.clone(), &headers).await;
    let mut response = blob_placements_response(&state.serving, audience, &digest);
    ProtectedCachePolicy::NoStore.apply(response.headers_mut());
    response
}

fn blob_placements_response(state: &ServingState, audience: AvailabilityAudience, digest: &str) -> Response {
    if audience != AvailabilityAudience::Administrator {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Ok(digest) = digest.parse::<ArtifactDigest>() else {
        return bad_request("invalid artifact digest");
    };
    let Ok(records) = state.meta.blob_placements(&digest) else {
        return internal_error();
    };
    let mut datacenters: Vec<BlobDatacenterPlacement> = records.iter().map(datacenter_placement).collect();
    datacenters.sort_by(|left, right| {
        left.data_center
            .cmp(&right.data_center)
            .then(left.updated_at.cmp(&right.updated_at))
    });
    axum::Json(BlobPlacementView {
        digest: digest.canonical(),
        datacenters,
    })
    .into_response()
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

const fn source_class(source: ArtifactSource) -> UiArtifactSource {
    match source {
        ArtifactSource::Hosted => UiArtifactSource::Hosted,
        ArtifactSource::Proxy => UiArtifactSource::Proxy,
        ArtifactSource::Generated => UiArtifactSource::Generated,
    }
}

const fn availability_class(availability: ByteAvailability) -> UiByteAvailability {
    match availability {
        ByteAvailability::Local => UiByteAvailability::Local,
        ByteAvailability::RemoteOnly => UiByteAvailability::RemoteOnly,
        ByteAvailability::Unavailable => UiByteAvailability::Unavailable,
    }
}

fn bad_request(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(serde_json::json!({ "error": message })),
    )
        .into_response()
}

fn internal_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(serde_json::json!({ "error": "placement query failed" })),
    )
        .into_response()
}
