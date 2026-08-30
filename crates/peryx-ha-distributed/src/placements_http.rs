use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse as _, Response};
use peryx_driver::state::{AppState, ServingState};
use peryx_ha::{
    AvailabilityAudience, AvailabilityPageQuery, AvailabilityViewReader, BlobPlacementViewError, PlacementViewError,
};
use peryx_http::response_security::ProtectedCachePolicy;

use crate::availability_http::{AvailabilityRejection, availability_audience};

const DEFAULT_PLACEMENT_LIMIT: usize = 25;

#[derive(Debug, serde::Deserialize)]
struct PlacementsQuery {
    cursor: Option<String>,
    limit: Option<usize>,
}

pub async fn placements(State(state): State<Arc<AppState>>, headers: HeaderMap, uri: Uri) -> Response {
    let Ok(audience) = availability_audience(state.serving.clone(), &headers).await else {
        return AvailabilityRejection::response();
    };
    let mut response = placements_response(&state.serving, audience, &uri);
    ProtectedCachePolicy::NoStore.apply(response.headers_mut());
    response
}

fn placements_response(state: &ServingState, audience: AvailabilityAudience, uri: &Uri) -> Response {
    if audience == AvailabilityAudience::Public {
        return StatusCode::FORBIDDEN.into_response();
    }
    let query = if audience == AvailabilityAudience::Administrator {
        let Ok(Query(query)) = Query::<PlacementsQuery>::try_from_uri(uri) else {
            return bad_request("invalid placement query");
        };
        AvailabilityPageQuery {
            cursor: query.cursor,
            limit: query.limit.unwrap_or(DEFAULT_PLACEMENT_LIMIT),
            include_rows: true,
        }
    } else {
        AvailabilityPageQuery {
            cursor: None,
            limit: DEFAULT_PLACEMENT_LIMIT,
            include_rows: false,
        }
    };
    match state.placement_view(query) {
        Ok(view) => axum::Json(view).into_response(),
        Err(PlacementViewError::InvalidLimit) => bad_request("placement limit out of range"),
        Err(PlacementViewError::HealthRead | PlacementViewError::RowsRead) => internal_error(),
    }
}

pub async fn blob_placements(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(digest): Path<String>,
) -> Response {
    let Ok(audience) = availability_audience(state.serving.clone(), &headers).await else {
        return AvailabilityRejection::response();
    };
    let mut response = blob_placements_response(&state.serving, audience, &digest);
    ProtectedCachePolicy::NoStore.apply(response.headers_mut());
    response
}

fn blob_placements_response(state: &ServingState, audience: AvailabilityAudience, digest: &str) -> Response {
    if audience != AvailabilityAudience::Administrator {
        return StatusCode::FORBIDDEN.into_response();
    }
    match state.blob_placement_view(digest) {
        Ok(view) => axum::Json(view).into_response(),
        Err(BlobPlacementViewError::InvalidDigest) => bad_request("invalid artifact digest"),
        Err(BlobPlacementViewError::Read) => internal_error(),
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
