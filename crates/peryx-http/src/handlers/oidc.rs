//! PyPI-compatible CI identity discovery and exchange.

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};

use peryx_driver::state::AppState;
use peryx_identity::{ExchangeError, ExchangedToken, IdentityExchange};

#[derive(Serialize)]
struct Audience<'a> {
    audience: &'a str,
}

pub async fn oidc_audience(Extension(runtime): Extension<Arc<dyn IdentityExchange>>) -> Response {
    Json(Audience {
        audience: runtime.audience(),
    })
    .into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MintRequest {
    pub(crate) token: String,
}

#[derive(Serialize)]
struct MintResponse {
    token: String,
    expires: i64,
}

pub async fn oidc_mint_token(
    State(state): State<Arc<AppState>>,
    Extension(runtime): Extension<Arc<dyn IdentityExchange>>,
    headers: HeaderMap,
    Json(request): Json<MintRequest>,
) -> Response {
    exchange_response(&headers, runtime.exchange(&request.token, (state.clock)()).await)
}

fn exchange_response(headers: &HeaderMap, result: Result<ExchangedToken, ExchangeError>) -> Response {
    match result {
        Ok(exchanged) => {
            peryx_events::security::Event::new("token_mint", "success")
                .actor(Some(&exchanged.publisher_id))
                .publisher_id(&exchanged.publisher_id)
                .token_id(&exchanged.token_id)
                .index(&exchanged.repository)
                .request(headers)
                .emit();
            (
                [(header::CACHE_CONTROL, "no-store"), (header::PRAGMA, "no-cache")],
                Json(MintResponse {
                    token: exchanged.token,
                    expires: exchanged.expires_at,
                }),
            )
                .into_response()
        }
        Err(error) => {
            let unavailable = error.unavailable();
            let status = if unavailable {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::UNPROCESSABLE_ENTITY
            };
            peryx_events::security::Event::new("token_mint", "denied")
                .reason(Some(if unavailable {
                    "identity provider unavailable"
                } else {
                    "identity rejected"
                }))
                .request(headers)
                .emit();
            (
                status,
                Json(serde_json::json!({
                    "message": if unavailable {
                        "identity provider unavailable"
                    } else {
                        "identity token rejected"
                    }
                })),
            )
                .into_response()
        }
    }
}
