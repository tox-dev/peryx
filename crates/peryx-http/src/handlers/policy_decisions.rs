use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use peryx_driver::state::AppState;
use peryx_identity::{Action, authorize_all};
use peryx_policy::PolicyDecisionState;
use peryx_storage::meta::{PolicyDecisionQuery, PolicyDecisionQueryError};

#[derive(Debug, serde::Deserialize)]
pub struct PolicyDecisionsQuery {
    repository: String,
    state: Option<PolicyDecisionState>,
    rule: Option<String>,
    source: Option<String>,
    from: Option<i64>,
    to: Option<i64>,
    cursor: Option<String>,
    limit: Option<usize>,
}

pub async fn policy_decisions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<PolicyDecisionsQuery>,
) -> Response {
    let Some(index) = state.indexes.iter().find(|index| index.route == query.repository) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let principal = index
        .acl
        .identify(
            headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok()),
            (state.clock)(),
        )
        .principal;
    if let Err(denial) = authorize_all(&principal, &index.acl, Action::Write) {
        return super::denied(denial);
    }
    match state.meta.query_policy_decisions(&PolicyDecisionQuery {
        repository: Some(index.name.clone()),
        state: query.state,
        rule: query.rule,
        source: query.source,
        evaluated_from_unix: query.from,
        evaluated_to_unix: query.to,
        cursor: query.cursor,
        limit: query.limit.unwrap_or(25),
    }) {
        Ok(page) => axum::Json(page).into_response(),
        Err(error) => policy_decision_error_response(&error),
    }
}

/// Keep validation failures actionable without exposing storage details.
#[must_use]
pub fn policy_decision_error_response(error: &PolicyDecisionQueryError) -> Response {
    let (status, message) = match error {
        PolicyDecisionQueryError::InvalidLimit | PolicyDecisionQueryError::InvalidCursor => {
            (StatusCode::BAD_REQUEST, error.to_string())
        }
        PolicyDecisionQueryError::Store(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "policy decision query failed".to_owned(),
        ),
    };
    (status, axum::Json(serde_json::json!({"error": message}))).into_response()
}
