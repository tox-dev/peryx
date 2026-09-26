use rstest::rstest;

use super::*;

fn anonymous(providers: &[&str], local: bool) -> UiLoginState {
    UiLoginState {
        user: None,
        password: false,
        providers: providers.iter().map(|&provider| provider.to_owned()).collect(),
        local,
    }
}

fn signed_in(password: bool) -> UiLoginState {
    UiLoginState {
        user: Some("Ada Lovelace".to_owned()),
        password,
        providers: Vec::new(),
        local: true,
    }
}

#[test]
fn test_login_view_lists_a_sign_in_link_per_provider() {
    let html = login_view(anonymous(&["corporate", "google"], false), false, None).to_html();
    assert!(html.contains("/_/login/corporate"), "{html}");
    assert!(html.contains("/_/login/google"), "{html}");
}

#[test]
fn test_login_view_shows_the_signed_in_user_and_a_logout_form() {
    let html = login_view(signed_in(false), false, None).to_html();
    assert!(html.contains("Ada Lovelace"), "{html}");
    assert!(html.contains("/_/logout"), "{html}");
    assert!(!html.contains("/_/login/password"), "{html}");
}

#[test]
fn test_login_view_without_a_way_to_sign_in_reports_none_configured() {
    let html = login_view(UiLoginState::default(), false, None).to_html();
    assert!(html.contains("No login providers are configured."), "{html}");
}

#[test]
fn test_login_view_offers_the_password_form_with_local_sign_in() {
    let html = login_view(anonymous(&[], true), false, None).to_html();
    assert!(html.contains(r#"action="/_/login/password""#), "{html}");
    assert!(html.contains(r#"name="name""#), "{html}");
    assert!(html.contains(r#"type="password" name="password""#), "{html}");
    assert!(!html.contains("No login providers are configured."), "{html}");
}

#[test]
fn test_login_view_offers_the_password_form_beside_providers() {
    let html = login_view(anonymous(&["corporate"], true), false, None).to_html();
    assert!(html.contains(r#"action="/_/login/password""#), "{html}");
    assert!(html.contains("/_/login/corporate"), "{html}");
}

#[test]
fn test_login_view_without_local_sign_in_omits_the_password_form() {
    let html = login_view(anonymous(&["corporate"], false), false, None).to_html();
    assert!(!html.contains("/_/login/password"), "{html}");
}

#[test]
fn test_login_view_reports_a_rejected_sign_in() {
    let html = login_view(anonymous(&[], true), true, None).to_html();
    assert!(html.contains("Sign-in failed. Check the name and password."), "{html}");
}

#[test]
fn test_login_view_reports_nothing_before_a_sign_in_attempt() {
    let html = login_view(anonymous(&[], true), false, None).to_html();
    assert!(!html.contains(r#"role="alert""#), "{html}");
}

#[test]
fn test_login_view_offers_a_local_account_the_password_change_form() {
    let html = login_view(signed_in(true), false, None).to_html();
    assert!(html.contains(r#"action="/_/password""#), "{html}");
    assert!(
        html.contains(r#"name="current_password" autocomplete="current-password""#),
        "{html}"
    );
    assert!(
        html.contains(r#"name="new_password" autocomplete="new-password""#),
        "{html}"
    );
    assert!(
        html.contains(r#"name="confirmation" autocomplete="new-password""#),
        "{html}"
    );
}

#[test]
fn test_login_view_offers_no_password_change_to_an_account_without_a_password() {
    let html = login_view(signed_in(false), false, None).to_html();
    assert!(!html.contains("/_/password"), "{html}");
}

#[rstest]
#[case::changed("changed", r#"role="status" class="dim">Password changed."#)]
#[case::rejected("rejected", "Password not changed. Check the current password.")]
#[case::invalid(
    "invalid",
    "Password not changed. The new password needs 15 to 1,024 characters and must match its confirmation."
)]
fn test_login_view_reports_the_password_change_outcome(#[case] query: &str, #[case] expected: &str) {
    let html = login_view(signed_in(true), false, PasswordChange::from_query(query)).to_html();
    assert!(html.contains(expected), "{html}");
}

#[test]
fn test_an_unknown_password_change_outcome_reports_nothing() {
    assert_eq!(PasswordChange::from_query("anything"), None);
}
