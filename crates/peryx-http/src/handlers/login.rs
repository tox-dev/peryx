//! These routes authenticate a human to the read-only web UI by sealing the resolved user into a
//! session cookie. They never mint a write credential: repository mutations stay on
//! `Authorization`-header tokens, so the session cookie carries no CSRF surface. The login handoff
//! (PKCE verifier, nonce, `state`) rides a single-use sealed pre-authentication cookie between the
//! redirect and the callback.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use peryx_driver::AppState;
use peryx_identity::{
    CallbackResponse, OidcLoginError, OidcProviderError, PRE_AUTH_COOKIE, PendingLogin, SESSION_COOKIE, ServerUser,
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
    Query(response): Query<CallbackResponse>,
    headers: HeaderMap,
) -> Response {
    let Some(service) = state.serving.oidc_login(&provider) else {
        return provider_not_found();
    };
    let Some(sealer) = state.serving.session_sealer() else {
        return misconfigured();
    };
    let now = (state.serving.clock)();
    let Some(pending) =
        read_cookie(&headers, PRE_AUTH_COOKIE).and_then(|value| sealer.open_pre_auth::<PendingLogin>(&value, now))
    else {
        return (
            StatusCode::BAD_REQUEST,
            "the login session is missing or has expired; start again",
        )
            .into_response();
    };
    match service.callback(&response, &pending, now).await {
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
    }
}

/// `GET /_/session` - the read-only UI's login state.
///
/// Reports the signed-in user (or null) and the OIDC providers a visitor can sign in with. The session
/// cookie is consulted only for identity here; it authorizes nothing that mutates state.
pub async fn session(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let user = session_user(&state, &headers)
        .map(|user| json!({ "id": user.id.as_str(), "name": user.name.display(), "state": user.state }));
    json_no_store(
        StatusCode::OK,
        &json!({ "user": user, "providers": state.serving.oidc_providers() }),
    )
}

/// Returns the session principal for read-only UI routes.
///
/// Session cookies never authorize mutations. Mutating routes require `Authorization` credentials.
#[must_use]
pub fn session_user(state: &AppState, headers: &HeaderMap) -> Option<ServerUser> {
    let now = (state.serving.clock)();
    let sealer = state.serving.session_sealer()?;
    let value = read_cookie(headers, SESSION_COOKIE)?;
    sealer.open_session(&value, now)
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

/// Map a provider-side failure to a response without leaking the provider's detail: an outage is a
/// retryable `503`, every rejection (bad state, nonce, signature, issuer, audience, redirect) a `401`.
fn provider_error_response(error: OidcProviderError) -> Response {
    if error.unavailable() {
        (StatusCode::SERVICE_UNAVAILABLE, "the login provider is unavailable").into_response()
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

fn read_cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    let header = headers.get(header::COOKIE)?.to_str().ok()?;
    header.split(';').find_map(|pair| {
        let (key, value) = pair.trim().split_once('=')?;
        (key == name).then(|| value.to_owned())
    })
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
