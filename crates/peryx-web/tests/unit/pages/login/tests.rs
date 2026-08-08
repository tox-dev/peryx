use super::*;

#[test]
fn test_login_view_lists_a_sign_in_link_per_provider() {
    let html = login_view(UiLoginState {
        user: None,
        providers: vec!["corporate".to_owned(), "google".to_owned()],
    })
    .to_html();
    assert!(html.contains("/_/login/corporate"), "{html}");
    assert!(html.contains("/_/login/google"), "{html}");
}

#[test]
fn test_login_view_shows_the_signed_in_user_and_a_logout_form() {
    let html = login_view(UiLoginState {
        user: Some("Ada Lovelace".to_owned()),
        providers: Vec::new(),
    })
    .to_html();
    assert!(html.contains("Ada Lovelace"), "{html}");
    assert!(html.contains("/_/logout"), "{html}");
}

#[test]
fn test_login_view_without_providers_reports_none_configured() {
    let html = login_view(UiLoginState::default()).to_html();
    assert!(html.contains("No login providers are configured."), "{html}");
}
