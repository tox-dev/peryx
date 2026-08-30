use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse as _, Response};
use peryx_driver::state::{AppState, ServingState};
use peryx_ha::{AvailabilityAudience, AvailabilityPageQuery, AvailabilityViewReader, OperationsViewError};
use peryx_http::response_security::ProtectedCachePolicy;

use crate::availability_http::{AvailabilityRejection, availability_audience};

const DEFAULT_OPERATION_LIMIT: usize = 25;

#[derive(Debug, serde::Deserialize)]
struct OperationsQuery {
    cursor: Option<String>,
    limit: Option<usize>,
}

pub async fn operations(State(state): State<Arc<AppState>>, headers: HeaderMap, uri: Uri) -> Response {
    let Ok(audience) = availability_audience(state.serving.clone(), &headers).await else {
        return AvailabilityRejection::response();
    };
    let mut response = operations_response(&state.serving, audience, &uri);
    ProtectedCachePolicy::NoStore.apply(response.headers_mut());
    response
}

fn operations_response(state: &ServingState, audience: AvailabilityAudience, uri: &Uri) -> Response {
    if audience == AvailabilityAudience::Public {
        return StatusCode::FORBIDDEN.into_response();
    }
    let query = if audience == AvailabilityAudience::Administrator {
        let Ok(Query(query)) = Query::<OperationsQuery>::try_from_uri(uri) else {
            return bad_request("invalid operation query");
        };
        AvailabilityPageQuery {
            cursor: query.cursor,
            limit: query.limit.unwrap_or(DEFAULT_OPERATION_LIMIT),
            include_rows: true,
        }
    } else {
        AvailabilityPageQuery {
            cursor: None,
            limit: DEFAULT_OPERATION_LIMIT,
            include_rows: false,
        }
    };
    match state.operations_view(query) {
        Ok(view) => axum::Json(view).into_response(),
        Err(OperationsViewError::InvalidLimit) => bad_request("operation limit out of range"),
        Err(OperationsViewError::HealthRead | OperationsViewError::RowsRead) => internal_error(),
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
        axum::Json(serde_json::json!({ "error": "operation query failed" })),
    )
        .into_response()
}
