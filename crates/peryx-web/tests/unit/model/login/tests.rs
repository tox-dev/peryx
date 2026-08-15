use super::UiLoginState;

#[test]
fn session_state_reads_valid_user_and_provider_names() {
    assert_eq!(
        UiLoginState::from_session(&serde_json::json!({
            "user": {"name": "Ada"},
            "providers": ["work", 7, "personal"],
        })),
        UiLoginState {
            user: Some("Ada".to_owned()),
            providers: vec!["work".to_owned(), "personal".to_owned()],
        }
    );
}

#[test]
fn session_state_defaults_missing_fields() {
    assert_eq!(
        UiLoginState::from_session(&serde_json::json!({})),
        UiLoginState::default()
    );
}
