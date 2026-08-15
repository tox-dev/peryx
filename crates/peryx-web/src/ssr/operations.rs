use std::sync::Arc;

use axum::http::HeaderMap;
use leptos::prelude::*;
use peryx_core::{OperationRow, OperationsHealth, OperationsView, UiOperationStatus};
use peryx_driver::AppState;
use peryx_http::response_security::FieldClassification;
use peryx_storage::meta::{
    OperationOutcomeHealth, OperationOutcomePage, OperationOutcomeQuery, OperationOutcomeRow, OperationState,
};

const DEFAULT_OPERATION_LIMIT: usize = 25;

/// # Errors
///
/// Returns a message when the caller lacks operator access or the store cannot be read.
pub async fn operations() -> Result<OperationsView, String> {
    let app = expect_context::<Arc<AppState>>();
    let headers = leptos_axum::extract::<HeaderMap>().await.unwrap_or_default();
    let class = peryx_http::handlers::status_authorization(&app, &headers)
        .await
        .field_class();
    if !matches!(
        class,
        Some(FieldClassification::Operator | FieldClassification::Administrator)
    ) {
        return Err("You do not have access to operation health.".to_owned());
    }
    let now = (app.serving.clock)();
    let health = app.serving.meta.operation_outcome_health(now).map_err(|_| ());
    let rows = if class == Some(FieldClassification::Administrator) {
        Some(
            app.serving
                .meta
                .list_operation_outcomes(&OperationOutcomeQuery {
                    cursor: None,
                    limit: DEFAULT_OPERATION_LIMIT,
                })
                .map_err(|_| ()),
        )
    } else {
        None
    };
    operations_for_class(now, health, rows)
}

fn operations_for_class(
    now: i64,
    health: Result<OperationOutcomeHealth, ()>,
    rows: Option<Result<OperationOutcomePage, ()>>,
) -> Result<OperationsView, String> {
    let Ok(health) = health else {
        return Err("Operation health could not be read.".to_owned());
    };
    let (rows, next_cursor) = if let Some(rows) = rows {
        let Ok(page) = rows else {
            return Err("Operation rows could not be read.".to_owned());
        };
        let mut projected = Vec::with_capacity(page.rows.len());
        for row in page.rows {
            projected.push(operation_row(row, now));
        }
        (Some(projected), page.next_cursor)
    } else {
        (None, None)
    };
    Ok(OperationsView {
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

#[cfg(test)]
#[path = "../../tests/unit/ssr/operations/tests.rs"]
mod tests;
