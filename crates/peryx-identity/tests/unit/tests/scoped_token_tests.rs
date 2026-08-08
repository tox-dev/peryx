use rstest::rstest;

use crate::{TokenId, TokenName, TokenNameError, TokenSecret, TokenVerifier};

#[test]
fn test_token_id_random_carries_the_prefix_and_is_unique() {
    let first = TokenId::random();
    let second = TokenId::random();
    assert!(first.as_str().starts_with("tok_"));
    assert_ne!(first, second);
    assert_eq!(first.to_string(), first.as_str());
}

#[test]
fn test_token_id_new_adopts_a_value_verbatim_and_round_trips() {
    let id = TokenId::new("tok_abc");
    assert_eq!(id.as_str(), "tok_abc");
    let json = serde_json::to_string(&id).unwrap();
    assert_eq!(json, "\"tok_abc\"");
    assert_eq!(serde_json::from_str::<TokenId>(&json).unwrap(), id);
}

#[rstest]
#[case::plain("ci-upload", "ci-upload")]
#[case::trimmed("  ci  ", "ci")]
fn test_token_name_preserves_the_trimmed_spelling(#[case] input: &str, #[case] expected: &str) {
    let name = TokenName::new(input).unwrap();
    assert_eq!(name.as_str(), expected);
    assert_eq!(serde_json::to_string(&name).unwrap(), format!("\"{expected}\""));
}

#[rstest]
#[case::empty("   ", TokenNameError::Empty)]
#[case::too_long(&"n".repeat(257), TokenNameError::TooLong)]
fn test_token_name_rejects_invalid_names(#[case] input: &str, #[case] expected: TokenNameError) {
    assert_eq!(TokenName::new(input).unwrap_err(), expected);
}

#[test]
fn test_token_name_accepts_the_maximum_length() {
    assert!(TokenName::new(&"n".repeat(256)).is_ok());
}

#[rstest]
#[case::empty(TokenNameError::Empty, "token name cannot be empty")]
#[case::too_long(TokenNameError::TooLong, "token name cannot exceed 256 bytes")]
fn test_token_name_error_messages(#[case] error: TokenNameError, #[case] expected: &str) {
    assert_eq!(error.to_string(), expected);
}

#[test]
fn test_token_secret_generate_is_prefixed_and_unique() {
    let first = TokenSecret::generate();
    let second = TokenSecret::generate();
    assert!(first.expose().starts_with("peryx_"));
    assert_ne!(first.expose(), second.expose());
}

#[test]
fn test_token_secret_verifier_is_deterministic_and_secret_specific() {
    let secret = TokenSecret::generate();
    assert_eq!(secret.verifier().as_str(), secret.verifier().as_str());
    let other = TokenSecret::generate();
    assert_ne!(secret.verifier().as_str(), other.verifier().as_str());
    assert_eq!(
        TokenSecret::presented(secret.expose()).verifier().as_str(),
        secret.verifier().as_str()
    );
}

#[test]
fn test_token_secret_debug_redacts() {
    let secret = TokenSecret::generate();
    let rendered = format!("{secret:?}");
    assert_eq!(rendered, "TokenSecret(<redacted>)");
    assert!(!rendered.contains(secret.expose()));
}

#[test]
fn test_token_verifier_debug_redacts_and_round_trips() {
    let verifier = TokenSecret::generate().verifier();
    let rendered = format!("{verifier:?}");
    assert_eq!(rendered, "TokenVerifier(<redacted>)");
    assert!(!rendered.contains(verifier.as_str()));
    let json = serde_json::to_string(&verifier).unwrap();
    assert_eq!(
        serde_json::from_str::<TokenVerifier>(&json).unwrap().as_str(),
        verifier.as_str()
    );
}
