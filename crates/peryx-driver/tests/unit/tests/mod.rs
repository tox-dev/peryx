mod access_tests;
mod authz_tests;
mod body_tests;
mod conditional_tests;
mod quota_tests;
mod revocation_tests;
mod state_tests;
mod tokens_tests;
mod user_tests;

#[test]
fn test_not_found_returns_plain_404() {
    assert_eq!(crate::not_found().status(), axum::http::StatusCode::NOT_FOUND);
}
