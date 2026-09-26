//! These routes authenticate a human to the read-only web UI by sealing the resolved user into a
//! session cookie. They never mint a write credential: repository mutations stay on
//! `Authorization`-header tokens. The one write a session reaches is its own user's password change,
//! which also demands the current password and a same-origin form. The login handoff
//! (PKCE verifier, nonce, `state`) rides a single-use sealed pre-authentication cookie between the
//! redirect and the callback.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse as _, Response};
use peryx_driver::AppState;
use peryx_driver::access::{origin_matches_host, read_cookie, session_user};
use peryx_identity::{
    CallbackResponse, MAX_PASSWORD_CHARACTERS, MIN_PASSWORD_CHARACTERS, OidcLoginError, OidcProviderError,
    PRE_AUTH_COOKIE, PendingLogin, SESSION_COOKIE,
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
/// Where a rejected password sign-in returns the browser. Every rejection lands here, so the page cannot
/// tell an unknown name from a wrong password.
const SIGN_IN_FAILED_PATH: &str = "/login?error=sign-in";
/// Where a password change returns the browser, by outcome. Every rejection of the current password or
/// the session shares one page, so it never says which check failed.
const PASSWORD_CHANGED_PATH: &str = "/login?password=changed";
const PASSWORD_REJECTED_PATH: &str = "/login?password=rejected";
const PASSWORD_INVALID_PATH: &str = "/login?password=invalid";
/// The fetch-metadata header a browser attaches to say how the request's initiator relates to its target.
const SEC_FETCH_SITE: &str = "sec-fetch-site";

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

/// `POST /_/login/password` - signs a local user in with the login page's name and password form.
///
/// Signing in plants a session, so a form another site submits could sign the browser into an account
/// the attacker controls (login CSRF). Only a same-origin submission reaches the password check.
pub async fn login_password(State(state): State<Arc<AppState>>, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    no_store(login_password_inner(&state, &uri, &headers, &body).await)
}

async fn login_password_inner(state: &AppState, uri: &Uri, headers: &HeaderMap, body: &[u8]) -> Response {
    let Some(sealer) = state.serving.session_sealer() else {
        return misconfigured();
    };
    if !same_origin(headers, uri) {
        return (StatusCode::FORBIDDEN, "cross-site sign-in rejected").into_response();
    }
    let Some((name, password)) = parse_password_form(body) else {
        return (StatusCode::BAD_REQUEST, "invalid sign-in form").into_response();
    };
    match state.serving.users.authenticate_account(&name, &password).await {
        Ok(Some(user)) => {
            let now = (state.serving.clock)();
            let session = sealer.seal_session(&user, now + SESSION_TTL_SECS);
            redirect(
                ROOT_PATH,
                &[set_cookie(SESSION_COOKIE, &session, ROOT_PATH, SESSION_TTL_SECS)],
            )
        }
        Ok(None) => redirect(SIGN_IN_FAILED_PATH, &[]),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "sign-in is unavailable; retry").into_response(),
    }
}

/// `POST /_/password` - replaces the signed-in local user's password from the login page's form.
///
/// The session names the account, and the form's current password must verify against it, so a stolen
/// cookie alone cannot change the password. The new password is checked against the length policy and
/// its confirmation before any derivation runs. The same-origin guard keeps another site from
/// submitting the form with the victim's cookie.
///
/// A change ends every session of the account, including one opened with a leaked old password. The
/// browser that made the change proved the current password, as a sign-in does, so it gets a fresh
/// session and stays signed in.
pub async fn change_password(
    State(state): State<Arc<AppState>>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    no_store(change_password_inner(&state, &uri, &headers, &body).await)
}

async fn change_password_inner(state: &AppState, uri: &Uri, headers: &HeaderMap, body: &[u8]) -> Response {
    if !same_origin(headers, uri) {
        return (StatusCode::FORBIDDEN, "cross-site password change rejected").into_response();
    }
    let Some(form) = parse_password_change_form(body) else {
        return (StatusCode::BAD_REQUEST, "invalid password form").into_response();
    };
    let Some((sealer, user)) = state
        .serving
        .session_sealer()
        .zip(session_user(&state.serving, headers))
    else {
        return redirect(PASSWORD_REJECTED_PATH, &[]);
    };
    if form.replacement != form.confirmation
        || !(MIN_PASSWORD_CHARACTERS..=MAX_PASSWORD_CHARACTERS).contains(&form.replacement.chars().count())
    {
        return redirect(PASSWORD_INVALID_PATH, &[]);
    }
    match state
        .serving
        .users
        .change_password(&user.id, &form.current, &form.replacement)
        .await
    {
        Ok(Some(user)) => {
            let session = sealer.seal_session(&user, (state.serving.clock)() + SESSION_TTL_SECS);
            redirect(
                PASSWORD_CHANGED_PATH,
                &[set_cookie(SESSION_COOKIE, &session, ROOT_PATH, SESSION_TTL_SECS)],
            )
        }
        Ok(None) => redirect(PASSWORD_REJECTED_PATH, &[]),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "password change is unavailable; retry").into_response(),
    }
}

struct PasswordChangeForm {
    current: String,
    replacement: String,
    confirmation: String,
}

fn parse_password_change_form(body: &[u8]) -> Option<PasswordChangeForm> {
    let (mut current, mut replacement, mut confirmation) = (None, None, None);
    for (key, value) in url::form_urlencoded::parse(body) {
        let field = match key.as_ref() {
            "current_password" => &mut current,
            "new_password" => &mut replacement,
            "confirmation" => &mut confirmation,
            _ => continue,
        };
        if field.replace(value.into_owned()).is_some() {
            return None;
        }
    }
    Some(PasswordChangeForm {
        current: current?,
        replacement: replacement?,
        confirmation: confirmation?,
    })
}

/// A browser that sends `Sec-Fetch-Site` decides on it alone. The UI's `Referrer-Policy: no-referrer`
/// makes that browser send `Origin: null` on a form post, so the origin carries nothing to compare.
/// Without fetch metadata the `Origin` must name the request's host. An HTTP/2 request names that host
/// in `:authority`, which arrives as the URI authority rather than a `Host` header.
fn same_origin(headers: &HeaderMap, uri: &Uri) -> bool {
    if let Some(site) = headers.get(SEC_FETCH_SITE) {
        return site == "same-origin";
    }
    let host = headers.get(header::HOST).cloned().or_else(|| {
        uri.authority()
            .and_then(|authority| HeaderValue::from_str(authority.as_str()).ok())
    });
    headers
        .get(header::ORIGIN)
        .is_some_and(|origin| origin_matches_host(origin, host.as_ref()))
}

fn parse_password_form(body: &[u8]) -> Option<(String, String)> {
    let (mut name, mut password) = (None, None);
    for (key, value) in url::form_urlencoded::parse(body) {
        let field = match key.as_ref() {
            "name" => &mut name,
            "password" => &mut password,
            _ => continue,
        };
        if field.replace(value.into_owned()).is_some() {
            return None;
        }
    }
    name.zip(password)
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
/// Reports the signed-in user (or null) and whether that user holds a local password to change, the
/// OIDC providers a visitor can sign in with, and whether the local name and password form works, which
/// it does whenever the server can seal a session. The session cookie is consulted only for identity
/// here; it authorizes nothing that mutates state.
pub async fn session(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let user = session_user(&state.serving, &headers).map(|user| {
        json!({
            "id": user.id.as_str(),
            "name": user.name.display(),
            "state": user.state,
            "password": state.serving.users.has_password(&user.id).is_ok_and(|password| password),
        })
    });
    json_no_store(
        StatusCode::OK,
        &json!({
            "user": user,
            "providers": state.serving.oidc_providers(),
            "local": state.serving.session_sealer().is_some(),
        }),
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
