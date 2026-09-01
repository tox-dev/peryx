use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};

use peryx_driver::client_address;
use peryx_driver::rate_limit::RouteClass;
use peryx_driver::{AppState, HttpRoutes, RouteDescriptor, RouteMethod, RoutePosture, RouteRateLimit, RouteSet};
use peryx_events::security::RequestContext;

use super::runtime::{ExchangeError, ExchangedToken, IdentityExchange};

pub(super) struct TrustedPublishingRoutes {
    runtime: Arc<dyn IdentityExchange>,
}

impl TrustedPublishingRoutes {
    pub(super) fn new(runtime: Arc<dyn IdentityExchange>) -> Self {
        Self { runtime }
    }
}

impl HttpRoutes for TrustedPublishingRoutes {
    fn routes(&self) -> RouteSet {
        RouteSet::new()
            .route(
                RouteDescriptor::new(
                    RouteMethod::Get,
                    "/_/oidc/audience",
                    RoutePosture::Read,
                    RouteRateLimit::Class(RouteClass::Authentication),
                ),
                get(oidc_audience),
            )
            .route(
                RouteDescriptor::new(
                    RouteMethod::Post,
                    "/_/oidc/mint-token",
                    RoutePosture::Mutation,
                    RouteRateLimit::Class(RouteClass::Authentication),
                ),
                post(oidc_mint_token).layer(DefaultBodyLimit::max(40 * 1024)),
            )
            .with_extension(self.runtime.clone())
    }
}

#[derive(Serialize)]
struct Audience<'a> {
    audience: &'a str,
}

async fn oidc_audience(Extension(runtime): Extension<Arc<dyn IdentityExchange>>) -> Response {
    Json(Audience {
        audience: runtime.audience(),
    })
    .into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MintRequest {
    token: String,
}

#[derive(Serialize)]
struct MintResponse {
    token: String,
    expires: i64,
}

async fn oidc_mint_token(
    State(state): State<Arc<AppState>>,
    Extension(runtime): Extension<Arc<dyn IdentityExchange>>,
    parts: axum::http::request::Parts,
    body: Bytes,
) -> Response {
    let Ok(request) = serde_json::from_slice::<MintRequest>(&body) else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({"message": "invalid request body"})),
        )
            .into_response();
    };
    exchange_response(
        RequestContext::new(&parts.headers, client_address::resolved(&parts.extensions)),
        runtime.exchange(&request.token, (state.serving.clock)()).await,
    )
}

fn exchange_response(request: RequestContext<'_>, result: Result<ExchangedToken, ExchangeError>) -> Response {
    match result {
        Ok(exchanged) => {
            emit_exchange_success(request, &exchanged);
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
            peryx_events::security::Event::new("token_mint", "denied")
                .reason(Some(if unavailable {
                    "identity provider unavailable"
                } else {
                    "identity rejected"
                }))
                .request(request)
                .emit();
            (
                if unavailable {
                    StatusCode::SERVICE_UNAVAILABLE
                } else {
                    StatusCode::UNPROCESSABLE_ENTITY
                },
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

fn emit_exchange_success(request: RequestContext<'_>, exchanged: &ExchangedToken) {
    let request_id = header_text(request.headers, "x-request-id");
    let user_agent = header_text(request.headers, header::USER_AGENT.as_str());
    let client_ip = request
        .client_ip
        .map(|client_ip| client_ip.to_string())
        .unwrap_or_default();
    let client_ip = client_ip.as_str();
    tracing::info!(
        target: "peryx::security",
        security_event = true,
        event = "index_action",
        action = "token_mint",
        result = "success",
        actor = exchanged.publisher_id,
        publisher_id = exchanged.publisher_id,
        token_id = exchanged.token_id,
        index = exchanged.repository,
        source_index = "",
        hosted_index = "",
        project = "",
        version = "",
        filename = "",
        digest = "",
        count = 0,
        changed = false,
        reason = "",
        request_id,
        user_agent,
        client_ip,
        "index security event"
    );
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers.get(name).and_then(|value| value.to_str().ok()).unwrap_or("")
}

#[cfg(test)]
#[path = "../../tests/unit/trusted_publishing/http_tests.rs"]
mod tests;
