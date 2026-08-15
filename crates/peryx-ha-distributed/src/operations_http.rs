use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse as _, Response};
use peryx_core::{OperationRow, OperationsHealth, OperationsView, UiOperationStatus};
use peryx_driver::state::{AppState, ServingState};
use peryx_ha::AvailabilityAudience;
use peryx_storage::meta::{OperationOutcomeQuery, OperationOutcomeQueryError, OperationOutcomeRow, OperationState};

use peryx_http::response_security::ProtectedCachePolicy;

use crate::availability_http::availability_audience;

const DEFAULT_OPERATION_LIMIT: usize = 25;

#[derive(Debug, serde::Deserialize)]
struct OperationsQuery {
    cursor: Option<String>,
    limit: Option<usize>,
}

pub async fn operations(State(state): State<Arc<AppState>>, headers: HeaderMap, uri: Uri) -> Response {
    let audience = availability_audience(state.serving.clone(), &headers).await;
    let mut response = operations_response(&state.serving, audience, &uri);
    ProtectedCachePolicy::NoStore.apply(response.headers_mut());
    response
}

fn operations_response(state: &ServingState, audience: AvailabilityAudience, uri: &Uri) -> Response {
    if audience == AvailabilityAudience::Public {
        return StatusCode::FORBIDDEN.into_response();
    }
    let now = (state.clock)();
    let mut rows = None;
    let mut next_cursor = None;
    if audience == AvailabilityAudience::Administrator {
        let Ok(Query(query)) = Query::<OperationsQuery>::try_from_uri(uri) else {
            return bad_request("invalid operation query");
        };
        let query = OperationOutcomeQuery {
            cursor: query.cursor,
            limit: query.limit.unwrap_or(DEFAULT_OPERATION_LIMIT),
        };
        match state.meta.list_operation_outcomes(&query) {
            Ok(page) => {
                rows = Some(page.rows.into_iter().map(|row| operation_row(row, now)).collect());
                next_cursor = page.next_cursor;
            }
            Err(OperationOutcomeQueryError::InvalidLimit) => return bad_request("operation limit out of range"),
            Err(OperationOutcomeQueryError::Store(_)) => return internal_error(),
        }
    }
    let Ok(health) = state.meta.operation_outcome_health(now) else {
        return internal_error();
    };
    axum::Json(OperationsView {
        captured_at: now,
        health: OperationsHealth {
            pending: health.pending,
            published: health.published,
            failed: health.failed,
            expired: health.expired,
            total: health.total(),
        },
        rows,
        next_cursor,
    })
    .into_response()
}

fn operation_row(row: OperationOutcomeRow, now: i64) -> OperationRow {
    OperationRow {
        operation: row.operation,
        status: UiOperationStatus::derive(
            matches!(row.state, OperationState::Published),
            matches!(row.state, OperationState::Failed),
            row.expiry_unix,
            now,
        ),
        updated_at: row.updated_at_unix,
        expires_at: row.expiry_unix,
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
