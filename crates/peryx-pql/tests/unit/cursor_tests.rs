use crate::cursor::{decode, encode};
use crate::error::PqlError;
use crate::scope::QueryScope;
use rstest::rstest;

use super::support::{operator_scope, repository_scope};

const CURSOR: &str = "eyJkb21haW4iOiJwb2xpY3kuZGVjaXNpb25zIiwic2NvcGUiOiI0NjRjNmQwZjM4OTQ2YjM3ZTRlMDNiNjg1OWYxNWFiMmM4OGQ1MTQ5MGVlOTkwMTllYzI4NjQ2OWI3NDNjY2E1Iiwib2Zmc2V0IjoyNX0";

#[test]
fn test_cursor_encodes_expected_payload() {
    assert_eq!(encode("policy.decisions", &operator_scope(), 25), CURSOR);
}

#[test]
fn test_cursor_decodes_expected_payload() {
    assert_eq!(decode(CURSOR, "policy.decisions", &operator_scope()), Ok(25));
}

#[rstest]
#[case::invalid_base64("!!!not base64!!!")]
#[case::invalid_json(&base64_of("hello"))]
fn test_cursor_rejects_malformed_text(#[case] text: &str) {
    assert_eq!(
        decode(text, "policy.decisions", &operator_scope()),
        Err(PqlError::InvalidCursor)
    );
}

#[test]
fn test_cursor_rejects_different_domain() {
    let scope = operator_scope();
    let cursor = encode("policy.decisions", &scope, 1);
    assert_eq!(decode(&cursor, "trash", &scope), Err(PqlError::InvalidCursor));
}

#[rstest]
#[case::different_repository(repository_scope("b"))]
#[case::operator(operator_scope())]
fn test_cursor_rejects_changed_scope(#[case] scope: QueryScope) {
    let cursor = encode("policy.decisions", &repository_scope("a"), 1);
    assert_eq!(
        decode(&cursor, "policy.decisions", &scope),
        Err(PqlError::CursorScopeChanged)
    );
}

fn base64_of(text: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(text)
}
