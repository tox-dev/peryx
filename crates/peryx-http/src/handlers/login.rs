//! These routes authenticate a human to the read-only web UI by sealing the resolved user into a
//! session cookie. They never mint a write credential: repository mutations stay on
//! `Authorization`-header tokens, so the session cookie carries no CSRF surface. The login handoff
//! (PKCE verifier, nonce, `state`) rides a single-use sealed pre-authentication cookie between the
//! redirect and the callback.

use std::sync::Arc;

use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use peryx_driver::AppState;
use peryx_driver::access::{read_cookie, session_user};
use peryx_identity::{
    CallbackResponse, OidcLoginError, OidcProviderError, PRE_AUTH_COOKIE, PendingLogin, SESSION_COOKIE,
};
use serde_json::json;

/// How long a browser session stays valid once created.
const SESSION_TTL_SECS: i64 = 12 * 60 * 60;
/// How long the pre-authentication handoff stays valid; the user must complete the redirect within it.
const PRE_AUTH_TTL_SECS: i64 = 10 * 60;
/// The pre-authentication cookie is scoped to the login routes, so it never rides an ordinary request.
const PRE_AUTH_PATH: &str = "/_/login";
/// Where a completed or cleared login lands the browser.
const ROOT_PATH: &str = "/";

pub async fn login_start(State(state): State<Arc<AppState>>, Path(provider): Path<String>) -> Response {
    let Some(service) = state.serving.oidc_login(&provider) else {
        return provider_not_found();
    };
    let Some(sealer) = state.serving.session_sealer() else {
        return misconfigured();
    };
    let now = (state.serving.clock)();
    match service.authorization(now).await {
        Ok(authorization) => {
            let handoff = sealer.seal_pre_auth(&authorization.pending, now + PRE_AUTH_TTL_SECS);
            redirect(
                authorization.redirect_url.as_str(),
                &[set_cookie(PRE_AUTH_COOKIE, &handoff, PRE_AUTH_PATH, PRE_AUTH_TTL_SECS)],
            )
        }
        Err(error) => provider_error_response(error),
    }
}

pub async fn login_callback(
    State(state): State<Arc<AppState>>,
    Path(provider): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    no_store(login_callback_inner(&state, &provider, query.as_deref(), &headers).await)
}

async fn login_callback_inner(state: &AppState, provider: &str, query: Option<&str>, headers: &HeaderMap) -> Response {
    let Some(response) = query.and_then(parse_callback_query) else {
        return (StatusCode::BAD_REQUEST, "invalid authentication response").into_response();
    };
    let Some(service) = state.serving.oidc_login(provider) else {
        return provider_not_found();
    };
    let Some(sealer) = state.serving.session_sealer() else {
        return misconfigured();
    };
    let now = (state.serving.clock)();
    let Some(pending) =
        read_cookie(headers, PRE_AUTH_COOKIE).and_then(|value| sealer.open_pre_auth::<PendingLogin>(&value, now))
    else {
        return rejected_handoff();
    };
    if !pending.matches_provider(service.id()) {
        return rejected_handoff();
    }
    match response {
        CallbackQuery::Code(response) => match service.callback(&response, &pending, now).await {
            Ok(resolution) => {
                let session = sealer.seal_session(&resolution.user, now + SESSION_TTL_SECS);
                redirect(
                    ROOT_PATH,
                    &[
                        set_cookie(SESSION_COOKIE, &session, ROOT_PATH, SESSION_TTL_SECS),
                        clear_cookie(PRE_AUTH_COOKIE, PRE_AUTH_PATH),
                    ],
                )
            }
            Err(error) => login_error_response(&error),
        },
        CallbackQuery::Error { state, error } => {
            if !pending.matches_state(&state) {
                return provider_error_response(OidcProviderError::StateMismatch);
            }
            let mut response = authorization_error_response(&error);
            response.headers_mut().append(
                header::SET_COOKIE,
                HeaderValue::from_str(&clear_cookie(PRE_AUTH_COOKIE, PRE_AUTH_PATH))
                    .expect("a cleared cookie is a valid header value"),
            );
            response
        }
    }
}

fn parse_callback_query(query: &str) -> Option<CallbackQuery> {
    let (mut state, mut code, mut error) = (None, None, None);
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        let field = match key.as_ref() {
            "state" => &mut state,
            "code" => &mut code,
            "error" => &mut error,
            _ => continue,
        };
        if field.replace(value.into_owned()).is_some() {
            return None;
        }
    }
    let state = state?;
    match (code, error) {
        (Some(code), None) => Some(CallbackQuery::Code(CallbackResponse { state, code })),
        (None, Some(error)) => Some(CallbackQuery::Error { state, error }),
        _ => None,
    }
}

enum CallbackQuery {
    Code(CallbackResponse),
    Error { state: String, error: String },
}

/// `GET /_/session` - the read-only UI's login state.
///
/// Reports the signed-in user (or null) and the OIDC providers a visitor can sign in with. The session
/// cookie is consulted only for identity here; it authorizes nothing that mutates state.
pub async fn session(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let user = session_user(&state.serving, &headers)
        .map(|user| json!({ "id": user.id.as_str(), "name": user.name.display(), "state": user.state }));
    json_no_store(
        StatusCode::OK,
        &json!({ "user": user, "providers": state.serving.oidc_providers() }),
    )
}

pub async fn logout() -> Response {
    redirect(ROOT_PATH, &[clear_cookie(SESSION_COOKIE, ROOT_PATH)])
}

fn provider_not_found() -> Response {
    (StatusCode::NOT_FOUND, "no OIDC login provider with that name").into_response()
}

fn misconfigured() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, "browser login is not configured").into_response()
}

fn rejected_handoff() -> Response {
    let mut response = (
        StatusCode::BAD_REQUEST,
        "the login session is missing or has expired; start again",
    )
        .into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(&clear_cookie(PRE_AUTH_COOKIE, PRE_AUTH_PATH))
            .expect("a cleared cookie is a valid header value"),
    );
    response
}

/// Only a valid `invalid_grant` response rejects authentication; upstream failures remain server errors.
fn provider_error_response(error: OidcProviderError) -> Response {
    if error.authentication_rejected() {
        (StatusCode::UNAUTHORIZED, "authentication failed").into_response()
    } else if error.unavailable() {
        (StatusCode::SERVICE_UNAVAILABLE, "the login provider is unavailable").into_response()
    } else if matches!(error, OidcProviderError::TokenExchange(_)) {
        (
            StatusCode::BAD_GATEWAY,
            "the login provider returned an invalid response",
        )
            .into_response()
    } else {
        (StatusCode::UNAUTHORIZED, "authentication failed").into_response()
    }
}

fn login_error_response<E>(error: &OidcLoginError<E>) -> Response {
    match error {
        OidcLoginError::Provider(provider) => provider_error_response(*provider),
        OidcLoginError::Store(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, "the login store is unavailable").into_response()
        }
    }
}

fn authorization_error_response(error: &str) -> Response {
    match error {
        "access_denied" => (StatusCode::UNAUTHORIZED, "authentication was denied").into_response(),
        "interaction_required" | "login_required" | "account_selection_required" | "consent_required" => {
            (StatusCode::UNAUTHORIZED, "authentication requires user interaction").into_response()
        }
        "server_error" | "temporarily_unavailable" => {
            (StatusCode::SERVICE_UNAVAILABLE, "the login provider is unavailable").into_response()
        }
        _ => (StatusCode::UNAUTHORIZED, "authentication failed").into_response(),
    }
}

fn set_cookie(name: &str, value: &str, path: &str, max_age: i64) -> String {
    format!("{name}={value}; Path={path}; Max-Age={max_age}; HttpOnly; Secure; SameSite=Lax")
}

fn clear_cookie(name: &str, path: &str) -> String {
    format!("{name}=; Path={path}; Max-Age=0; HttpOnly; Secure; SameSite=Lax")
}

/// A `303` redirect to `location` that sets each cookie and is never cached, so a login response with a
/// `Set-Cookie` is not stored by a shared cache.
fn redirect(location: &str, cookies: &[String]) -> Response {
    let mut response = StatusCode::SEE_OTHER.into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::LOCATION,
        HeaderValue::from_str(location).expect("a redirect target is a valid header value"),
    );
    for cookie in cookies {
        headers.append(
            header::SET_COOKIE,
            HeaderValue::from_str(cookie).expect("a sealed cookie is a valid header value"),
        );
    }
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn json_no_store(status: StatusCode, body: &serde_json::Value) -> Response {
    (status, [(header::CACHE_CONTROL, "no-store")], axum::Json(body)).into_response()
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}
