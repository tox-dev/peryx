//! peryx has no server-wide administrator; authority is per index. The gate is therefore the index's
//! own: the caller must present a Basic credential with write access to every resource (`*`). That
//! principal may read who else holds a grant, but never a token secret.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};

use peryx_driver::state::AppState;
use peryx_identity::{Action, authorize_all};

use crate::response_security::ProtectedCachePolicy;

/// The index whose ACL to describe, by its route.
#[derive(Debug, serde::Deserialize)]
pub struct AclQuery {
    index: String,
}

/// Answers `404` for an unknown route, `401`/`403` for a caller that does not administer the index, and
/// never returns a token secret, only a redacted marker that one exists.
pub async fn acl(State(state): State<Arc<AppState>>, headers: HeaderMap, Query(query): Query<AclQuery>) -> Response {
    let Some(index) = state.serving.indexes.iter().find(|index| index.route == query.index) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let credential = headers.get(header::AUTHORIZATION).and_then(|value| value.to_str().ok());
    let principal = index.acl.identify(credential, (state.serving.clock)()).principal;
    if let Err(denial) = authorize_all(&principal, &index.acl, Action::Write) {
        return super::denied(denial);
    }
    let tokens: Vec<serde_json::Value> = index
        .acl
        .tokens
        .iter()
        .map(|token| {
            serde_json::json!({
                "name": token.name,
                "secret": {"configured": true, "redacted": "<redacted>"},
                "expires_at": token.expires_at,
                "grants": token.grants,
            })
        })
        .collect();
    let mut response = axum::Json(serde_json::json!({
        "index": index.name,
        "route": index.route,
        "anonymous_read": index.acl.anonymous_read,
        "tokens": tokens,
    }))
    .into_response();
    ProtectedCachePolicy::NoStore.apply(response.headers_mut());
    response
}
