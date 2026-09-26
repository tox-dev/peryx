use super::*;

fn anonymous(providers: &[&str], local: bool) -> UiLoginState {
    UiLoginState {
        user: None,
        providers: providers.iter().map(|&provider| provider.to_owned()).collect(),
        local,
    }
}

#[test]
fn test_login_view_lists_a_sign_in_link_per_provider() {
    let html = login_view(anonymous(&["corporate", "google"], false), false).to_html();
    assert!(html.contains("/_/login/corporate"), "{html}");
    assert!(html.contains("/_/login/google"), "{html}");
}

#[test]
fn test_login_view_shows_the_signed_in_user_and_a_logout_form() {
    let html = login_view(
        UiLoginState {
            user: Some("Ada Lovelace".to_owned()),
            providers: Vec::new(),
            local: true,
        },
        false,
    )
    .to_html();
    assert!(html.contains("Ada Lovelace"), "{html}");
    assert!(html.contains("/_/logout"), "{html}");
    assert!(!html.contains("/_/login/password"), "{html}");
}

#[test]
fn test_login_view_without_a_way_to_sign_in_reports_none_configured() {
    let html = login_view(UiLoginState::default(), false).to_html();
    assert!(html.contains("No login providers are configured."), "{html}");
}

#[test]
fn test_login_view_offers_the_password_form_with_local_sign_in() {
    let html = login_view(anonymous(&[], true), false).to_html();
    assert!(html.contains(r#"action="/_/login/password""#), "{html}");
    assert!(html.contains(r#"name="name""#), "{html}");
    assert!(html.contains(r#"type="password" name="password""#), "{html}");
    assert!(!html.contains("No login providers are configured."), "{html}");
}

#[test]
fn test_login_view_offers_the_password_form_beside_providers() {
    let html = login_view(anonymous(&["corporate"], true), false).to_html();
    assert!(html.contains(r#"action="/_/login/password""#), "{html}");
    assert!(html.contains("/_/login/corporate"), "{html}");
}

#[test]
fn test_login_view_without_local_sign_in_omits_the_password_form() {
    let html = login_view(anonymous(&["corporate"], false), false).to_html();
    assert!(!html.contains("/_/login/password"), "{html}");
}

#[test]
fn test_login_view_reports_a_rejected_sign_in() {
    let html = login_view(anonymous(&[], true), true).to_html();
    assert!(html.contains("Sign-in failed. Check the name and password."), "{html}");
}

#[test]
fn test_login_view_reports_nothing_before_a_sign_in_attempt() {
    let html = login_view(anonymous(&[], true), false).to_html();
    assert!(!html.contains(r#"role="alert""#), "{html}");
}
