use std::sync::Arc;

use axum::body::Body;
use axum::http::{Method, Request, Response, StatusCode, header};
use peryx_driver::state::AppState;
use peryx_driver::users::UserService;
use peryx_identity::{PasswordPolicy, SESSION_COOKIE, ServerUser, SessionSealer};
use rstest::rstest;
use serde_json::Value;
use tower::ServiceExt as _;

pub(super) const KEY: &[u8] = b"a-token-realm-signing-secret-here";
pub(super) const NOW: i64 = 4_102_444_800 - 600;
pub(super) const PASSWORD: &str = "correct horse battery";
pub(super) const ORIGIN: &str = "https://peryx.test";
pub(super) const HOST: &str = "peryx.test";
const FORM: &str = "application/x-www-form-urlencoded";

pub(super) struct Fixture {
    _dir: tempfile::TempDir,
    pub(super) state: Arc<AppState>,
    pub(super) user: ServerUser,
}

pub(super) async fn fixture() -> Fixture {
    fixture_with(true, 2).await
}

pub(super) async fn fixture_with(sealer: bool, password_checks: usize) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let meta = peryx_storage::meta::MetaStore::open(dir.path().join("peryx.redb")).unwrap();
    let mut state = AppState::with_clock(
        meta.clone(),
        peryx_storage::blob::BlobStore::new(dir.path().join("blobs")),
        60,
        Vec::new(),
        Arc::new(|| NOW),
    );
    if sealer {
        assert!(state.set_session_sealer(SessionSealer::new(KEY)).is_ok());
    }
    let enrollment = UserService::with_password_settings(meta.clone(), PasswordPolicy::new(8, 1, 1).unwrap(), 2);
    let user = enrollment.create("Ada Lovelace").unwrap();
    enrollment.set_password(&user.id, PASSWORD).await.unwrap();
    Arc::get_mut(&mut state.serving).unwrap().users =
        UserService::with_password_settings(meta, PasswordPolicy::new(8, 1, 1).unwrap(), password_checks);
    Fixture {
        _dir: dir,
        state: Arc::new(state),
        user,
    }
}

pub(super) fn form(name: &str, password: &str) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .append_pair("name", name)
        .append_pair("password", password)
        .finish()
}

pub(super) async fn post(state: &Arc<AppState>, uri: &str, headers: &[(&str, &str)], body: String) -> Response<Body> {
    let mut request = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::CONTENT_TYPE, FORM);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    crate::router(state.clone())
        .oneshot(request.body(Body::from(body)).unwrap())
        .await
        .unwrap()
}

pub(super) async fn sign_in(state: &Arc<AppState>, body: String) -> Response<Body> {
    post(
        state,
        "/_/login/password",
        &[("host", HOST), ("origin", ORIGIN), ("sec-fetch-site", "same-origin")],
        body,
    )
    .await
}

pub(super) fn set_cookies(response: &Response<Body>) -> Vec<String> {
    response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|value| value.to_str().unwrap().to_owned())
        .collect()
}

pub(super) fn session_value(response: &Response<Body>) -> String {
    let cookies = set_cookies(response);
    assert_eq!(cookies.len(), 1, "{cookies:?}");
    cookies[0]
        .strip_prefix(&format!("{SESSION_COOKIE}="))
        .and_then(|cookie| cookie.split(';').next())
        .unwrap()
        .to_owned()
}

pub(super) fn location(response: &Response<Body>) -> &str {
    response.headers()[header::LOCATION].to_str().unwrap()
}

pub(super) async fn body_text(response: Response<Body>) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn test_password_sign_in_redirects_home_with_a_sealed_session() {
    let fixture = fixture().await;

    let response = sign_in(&fixture.state, form("ada lovelace", PASSWORD)).await;

    assert_eq!((response.status(), location(&response)), (StatusCode::SEE_OTHER, "/"));
    assert_eq!(
        SessionSealer::new(KEY).open_session(&session_value(&response), NOW),
        Some(fixture.user)
    );
}

#[tokio::test]
async fn test_password_sign_in_sets_the_session_cookie_attributes() {
    let fixture = fixture().await;

    let response = sign_in(&fixture.state, form("Ada Lovelace", PASSWORD)).await;

    let cookies = set_cookies(&response);
    assert!(
        cookies[0].ends_with("; Path=/; Max-Age=43200; HttpOnly; Secure; SameSite=Lax"),
        "{cookies:?}"
    );
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
}

#[tokio::test]
async fn test_a_password_session_reads_back_as_the_signed_in_user() {
    let fixture = fixture().await;
    let signed_in = sign_in(&fixture.state, form("Ada Lovelace", PASSWORD)).await;
    let cookie = format!("{SESSION_COOKIE}={}", session_value(&signed_in));

    let response = crate::router(fixture.state.clone())
        .oneshot(
            Request::get("/_/session")
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["user"]["name"], "Ada Lovelace");
}

#[rstest]
#[case::wrong_password("Ada Lovelace", "not the password", false)]
#[case::unknown_user("Grace Hopper", PASSWORD, false)]
#[case::disabled_user("Ada Lovelace", PASSWORD, true)]
#[tokio::test]
async fn test_a_rejected_password_sign_in_returns_to_the_login_page_without_a_session(
    #[case] name: &str,
    #[case] password: &str,
    #[case] disable: bool,
) {
    let fixture = fixture().await;
    if disable {
        fixture.state.serving.users.disable(&fixture.user.id).unwrap();
    }

    let response = sign_in(&fixture.state, form(name, password)).await;

    assert_eq!(
        (response.status(), location(&response)),
        (StatusCode::SEE_OTHER, "/login?error=sign-in")
    );
    assert_eq!(set_cookies(&response), Vec::<String>::new());
}

#[rstest]
#[case::cross_site(&[("host", HOST), ("origin", "https://attacker.test"), ("sec-fetch-site", "cross-site")])]
#[case::same_site(&[("host", HOST), ("origin", ORIGIN), ("sec-fetch-site", "same-site")])]
#[case::direct_navigation(&[("host", HOST), ("origin", ORIGIN), ("sec-fetch-site", "none")])]
#[case::other_origin_without_fetch_metadata(&[("host", HOST), ("origin", "https://attacker.test")])]
#[case::null_origin_without_fetch_metadata(&[("host", HOST), ("origin", "null")])]
#[case::no_browser_context(&[("host", HOST)])]
#[tokio::test]
async fn test_a_cross_site_password_sign_in_is_rejected(#[case] headers: &[(&str, &str)]) {
    let fixture = fixture().await;

    let response = post(
        &fixture.state,
        "/_/login/password",
        headers,
        form("Ada Lovelace", PASSWORD),
    )
    .await;

    assert_eq!(set_cookies(&response), Vec::<String>::new());
    assert_eq!(
        (response.status(), body_text(response).await.as_str()),
        (StatusCode::FORBIDDEN, "cross-site sign-in rejected")
    );
}

/// The login page sends `Referrer-Policy: no-referrer`, so a browser posting its form sends
/// `Origin: null` and lets `Sec-Fetch-Site` say where the request came from.
#[rstest]
#[case::browser_form(&[("host", HOST), ("origin", "null"), ("sec-fetch-site", "same-origin")])]
#[case::origin_without_fetch_metadata(&[("host", HOST), ("origin", ORIGIN)])]
#[tokio::test]
async fn test_a_same_origin_password_sign_in_is_accepted(#[case] headers: &[(&str, &str)]) {
    let fixture = fixture().await;

    let response = post(
        &fixture.state,
        "/_/login/password",
        headers,
        form("Ada Lovelace", PASSWORD),
    )
    .await;

    assert_eq!(location(&response), "/");
}

/// An HTTP/2 request carries its target in `:authority` rather than a `Host` header.
#[tokio::test]
async fn test_password_sign_in_matches_the_origin_against_the_request_authority() {
    let fixture = fixture().await;

    let response = post(
        &fixture.state,
        "https://peryx.test/_/login/password",
        &[("origin", ORIGIN)],
        form("Ada Lovelace", PASSWORD),
    )
    .await;

    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(location(&response), "/");
}

#[rstest]
#[case::missing_password("name=Ada+Lovelace")]
#[case::missing_name("password=secret")]
#[case::repeated_name("name=Ada&name=Grace&password=secret")]
#[case::repeated_password("name=Ada&password=secret&password=other")]
#[tokio::test]
async fn test_an_invalid_password_form_is_a_bad_request(#[case] body: &str) {
    let fixture = fixture().await;

    let response = sign_in(&fixture.state, body.to_owned()).await;

    assert_eq!(
        (response.status(), body_text(response).await.as_str()),
        (StatusCode::BAD_REQUEST, "invalid sign-in form")
    );
}

#[tokio::test]
async fn test_password_sign_in_ignores_fields_it_does_not_read() {
    let fixture = fixture().await;

    let response = sign_in(
        &fixture.state,
        format!("remember=on&{}", form("Ada Lovelace", PASSWORD)),
    )
    .await;

    assert_eq!(location(&response), "/");
}

#[tokio::test]
async fn test_password_sign_in_without_a_sealer_is_a_server_error() {
    let fixture = fixture_with(false, 2).await;

    let response = sign_in(&fixture.state, form("Ada Lovelace", PASSWORD)).await;

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn test_password_sign_in_reports_an_exhausted_password_check_as_unavailable() {
    let fixture = fixture_with(true, 0).await;

    let response = sign_in(&fixture.state, form("Ada Lovelace", PASSWORD)).await;

    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert_eq!(
        (response.status(), body_text(response).await.as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, "sign-in is unavailable; retry")
    );
}

#[rstest]
#[case::with_a_sealer(true)]
#[case::without_a_sealer(false)]
#[tokio::test]
async fn test_session_reports_whether_local_sign_in_works(#[case] sealer: bool) {
    let fixture = fixture_with(sealer, 2).await;

    let response = crate::router(fixture.state.clone())
        .oneshot(Request::get("/_/session").body(Body::empty()).unwrap())
        .await
        .unwrap();

    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["local"], sealer);
}
