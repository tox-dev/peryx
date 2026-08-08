use crate::cursor::{decode, encode};
use crate::error::PqlError;

use super::support::{operator_scope, repository_scope};

#[test]
fn test_cursor_round_trips_within_scope() {
    let scope = operator_scope();
    let cursor = encode("policy.decisions", &scope, 25);
    assert_eq!(decode(&cursor, "policy.decisions", &scope), Ok(25));
}

#[test]
fn test_cursor_rejects_malformed_text() {
    let scope = operator_scope();
    assert_eq!(
        decode("!!!not base64!!!", "policy.decisions", &scope),
        Err(PqlError::InvalidCursor)
    );
    let not_json = base64_of("hello");
    assert_eq!(
        decode(&not_json, "policy.decisions", &scope),
        Err(PqlError::InvalidCursor)
    );
}

#[test]
fn test_cursor_rejects_different_domain() {
    let scope = operator_scope();
    let cursor = encode("policy.decisions", &scope, 1);
    assert_eq!(decode(&cursor, "trash", &scope), Err(PqlError::InvalidCursor));
}

#[test]
fn test_cursor_rejects_changed_scope() {
    let cursor = encode("policy.decisions", &repository_scope("a"), 1);
    assert_eq!(
        decode(&cursor, "policy.decisions", &repository_scope("b")),
        Err(PqlError::CursorScopeChanged)
    );
    assert_eq!(
        decode(&cursor, "policy.decisions", &operator_scope()),
        Err(PqlError::CursorScopeChanged)
    );
}

fn base64_of(text: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(text)
}
