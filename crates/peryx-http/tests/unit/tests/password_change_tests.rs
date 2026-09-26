use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, Response, StatusCode, header};
use peryx_driver::state::AppState;
use peryx_identity::{PasswordVerifier, SESSION_COOKIE, ServerUser, SessionSealer};
use rstest::rstest;
use serde_json::Value;
use tower::ServiceExt as _;

use super::password_login_tests::{
    HOST, KEY, NOW, ORIGIN, PASSWORD, body_text, fixture, fixture_with, form, location, post, session_value,
    set_cookies, sign_in,
};

const NEW_PASSWORD: &str = "a much longer passphrase";

fn cookie(user: &ServerUser) -> String {
    format!(
        "{SESSION_COOKIE}={}",
        SessionSealer::new(KEY).seal_session(user, NOW + 3_600)
    )
}

fn change_form(current: &str, replacement: &str, confirmation: &str) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .append_pair("current_password", current)
        .append_pair("new_password", replacement)
        .append_pair("confirmation", confirmation)
        .finish()
}

async fn change(state: &Arc<AppState>, cookie: &str, body: String) -> Response<Body> {
    post(
        state,
        "/_/password",
        &[
            ("host", HOST),
            ("origin", ORIGIN),
            ("sec-fetch-site", "same-origin"),
            ("cookie", cookie),
        ],
        body,
    )
    .await
}

fn verifier(state: &AppState, user: &ServerUser) -> Option<PasswordVerifier> {
    state
        .serving
        .meta
        .get_user_password(&user.id)
        .unwrap()
        .map(|stored| stored.verifier().clone())
}

#[tokio::test]
async fn test_password_change_redirects_to_the_changed_status() {
    let fixture = fixture().await;

    let response = change(
        &fixture.state,
        &cookie(&fixture.user),
        change_form(PASSWORD, NEW_PASSWORD, NEW_PASSWORD),
    )
    .await;

    assert_eq!(
        (
            response.status(),
            location(&response),
            response.headers()[header::CACHE_CONTROL].to_str().unwrap()
        ),
        (StatusCode::SEE_OTHER, "/login?password=changed", "no-store")
    );
    assert_eq!(set_cookies(&response), Vec::<String>::new());
}

#[tokio::test]
async fn test_a_changed_password_signs_in_and_the_old_one_does_not() {
    let fixture = fixture().await;
    let signed_in = sign_in(&fixture.state, form("Ada Lovelace", PASSWORD)).await;
    let session = format!("{SESSION_COOKIE}={}", session_value(&signed_in));

    change(
        &fixture.state,
        &session,
        change_form(PASSWORD, NEW_PASSWORD, NEW_PASSWORD),
    )
    .await;

    let with_new = sign_in(&fixture.state, form("Ada Lovelace", NEW_PASSWORD)).await;
    let with_old = sign_in(&fixture.state, form("Ada Lovelace", PASSWORD)).await;
    assert_eq!(
        (location(&with_new), location(&with_old)),
        ("/", "/login?error=sign-in")
    );
}

#[rstest]
#[case::wrong_current_password(Session::Active, "not the password")]
#[case::no_session(Session::Absent, PASSWORD)]
#[case::disabled_user(Session::Disabled, PASSWORD)]
#[case::user_without_a_password(Session::Passwordless, PASSWORD)]
#[tokio::test]
async fn test_a_rejected_password_change_leaves_the_password(#[case] session: Session, #[case] current: &str) {
    let fixture = fixture().await;
    let users = &fixture.state.serving.users;
    let (user, cookie) = match session {
        Session::Active => (fixture.user.clone(), cookie(&fixture.user)),
        Session::Absent => (fixture.user.clone(), String::new()),
        Session::Disabled => {
            users.disable(&fixture.user.id).unwrap();
            (fixture.user.clone(), cookie(&fixture.user))
        }
        Session::Passwordless => {
            let provider_user = users.create("Grace Hopper").unwrap();
            let cookie = cookie(&provider_user);
            (provider_user, cookie)
        }
    };
    let before = verifier(&fixture.state, &user);

    let response = change(
        &fixture.state,
        &cookie,
        change_form(current, NEW_PASSWORD, NEW_PASSWORD),
    )
    .await;

    assert_eq!(
        (response.status(), location(&response)),
        (StatusCode::SEE_OTHER, "/login?password=rejected")
    );
    assert_eq!(verifier(&fixture.state, &user), before);
}

#[derive(Clone, Copy)]
enum Session {
    Active,
    Absent,
    Disabled,
    Passwordless,
}

#[rstest]
#[case::too_short(&"a".repeat(14), &"a".repeat(14))]
#[case::too_long(&"a".repeat(1_025), &"a".repeat(1_025))]
#[case::too_short_in_characters_though_long_in_bytes(&"é".repeat(14), &"é".repeat(14))]
#[case::confirmation_mismatch(NEW_PASSWORD, "a different passphrase")]
#[tokio::test]
async fn test_an_invalid_new_password_is_refused_before_the_current_one_is_checked(
    #[case] replacement: &str,
    #[case] confirmation: &str,
) {
    // No password check can run, so reaching the current-password check would answer 503 instead.
    let fixture = fixture_with(true, 0).await;
    let before = verifier(&fixture.state, &fixture.user);

    let response = change(
        &fixture.state,
        &cookie(&fixture.user),
        change_form(PASSWORD, replacement, confirmation),
    )
    .await;

    assert_eq!(
        (location(&response), verifier(&fixture.state, &fixture.user)),
        ("/login?password=invalid", before)
    );
}

/// The policy counts Unicode characters, not bytes, at both ends of its range.
#[rstest]
#[case::shortest(&"é".repeat(15))]
#[case::longest(&"a".repeat(1_024))]
#[tokio::test]
async fn test_a_new_password_at_the_policy_bounds_is_accepted(#[case] replacement: &str) {
    let fixture = fixture().await;

    let response = change(
        &fixture.state,
        &cookie(&fixture.user),
        change_form(PASSWORD, replacement, replacement),
    )
    .await;

    assert_eq!(location(&response), "/login?password=changed");
}

#[rstest]
#[case::cross_site(&[("host", HOST), ("origin", "https://attacker.test"), ("sec-fetch-site", "cross-site")])]
#[case::same_site(&[("host", HOST), ("origin", ORIGIN), ("sec-fetch-site", "same-site")])]
#[case::direct_navigation(&[("host", HOST), ("origin", ORIGIN), ("sec-fetch-site", "none")])]
#[case::other_origin_without_fetch_metadata(&[("host", HOST), ("origin", "https://attacker.test")])]
#[case::null_origin_without_fetch_metadata(&[("host", HOST), ("origin", "null")])]
#[case::no_browser_context(&[("host", HOST)])]
#[tokio::test]
async fn test_a_cross_site_password_change_is_rejected(#[case] headers: &[(&str, &str)]) {
    let fixture = fixture().await;
    let before = verifier(&fixture.state, &fixture.user);
    let session = cookie(&fixture.user);
    let mut headers = headers.to_vec();
    headers.push(("cookie", &session));

    let response = post(
        &fixture.state,
        "/_/password",
        &headers,
        change_form(PASSWORD, NEW_PASSWORD, NEW_PASSWORD),
    )
    .await;

    assert_eq!(verifier(&fixture.state, &fixture.user), before);
    assert_eq!(
        (response.status(), body_text(response).await.as_str()),
        (StatusCode::FORBIDDEN, "cross-site password change rejected")
    );
}

#[rstest]
#[case::missing_current("new_password=x&confirmation=x")]
#[case::missing_new("current_password=x&confirmation=x")]
#[case::missing_confirmation("current_password=x&new_password=x")]
#[case::repeated_field("current_password=x&current_password=y&new_password=x&confirmation=x")]
#[tokio::test]
async fn test_an_invalid_password_change_form_is_a_bad_request(#[case] body: &str) {
    let fixture = fixture().await;

    let response = change(&fixture.state, &cookie(&fixture.user), body.to_owned()).await;

    assert_eq!(
        (response.status(), body_text(response).await.as_str()),
        (StatusCode::BAD_REQUEST, "invalid password form")
    );
}

#[tokio::test]
async fn test_a_password_change_ignores_fields_it_does_not_read() {
    let fixture = fixture().await;

    let response = change(
        &fixture.state,
        &cookie(&fixture.user),
        format!("remember=on&{}", change_form(PASSWORD, NEW_PASSWORD, NEW_PASSWORD)),
    )
    .await;

    assert_eq!(location(&response), "/login?password=changed");
}

#[tokio::test]
async fn test_a_password_change_reports_an_exhausted_password_check_as_unavailable() {
    let fixture = fixture_with(true, 0).await;

    let response = change(
        &fixture.state,
        &cookie(&fixture.user),
        change_form(PASSWORD, NEW_PASSWORD, NEW_PASSWORD),
    )
    .await;

    assert_eq!(
        (response.status(), body_text(response).await.as_str()),
        (StatusCode::SERVICE_UNAVAILABLE, "password change is unavailable; retry")
    );
}

#[tokio::test]
async fn test_a_read_only_replica_refuses_a_password_change() {
    let mut fixture = fixture().await;
    Arc::get_mut(&mut fixture.state).unwrap().set_read_only(true).unwrap();
    let before = verifier(&fixture.state, &fixture.user);

    let response = change(
        &fixture.state,
        &cookie(&fixture.user),
        change_form(PASSWORD, NEW_PASSWORD, NEW_PASSWORD),
    )
    .await;

    assert_eq!(
        (response.status(), verifier(&fixture.state, &fixture.user)),
        (StatusCode::SERVICE_UNAVAILABLE, before)
    );
}

#[rstest]
#[case::local_password(true)]
#[case::provider_only(false)]
#[tokio::test]
async fn test_session_reports_whether_the_user_holds_a_password(#[case] password: bool) {
    let fixture = fixture().await;
    if !password {
        fixture.state.serving.users.clear_password(&fixture.user.id).unwrap();
    }

    let response = crate::router(fixture.state.clone())
        .oneshot(
            Request::get("/_/session")
                .header(header::COOKIE, cookie(&fixture.user))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["user"]["password"], password);
}
