use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use crate::{authorized, parse_basic};

fn basic(credentials: &[u8]) -> String {
    format!("Basic {}", STANDARD.encode(credentials))
}

#[test]
fn test_authorized_accepts_any_user_with_the_token() {
    assert!(authorized(Some(&basic(b"__token__:s3cret")), "s3cret"));
    assert!(authorized(Some(&basic(b"alice:s3cret")), "s3cret"));
}

#[test]
fn test_authorized_rejects_wrong_password() {
    assert!(!authorized(Some(&basic(b"alice:nope")), "s3cret"));
}

#[test]
fn test_authorized_rejects_missing_or_non_basic_header() {
    assert!(!authorized(None, "s3cret"));
    assert!(!authorized(Some("Bearer s3cret"), "s3cret"));
}

#[test]
fn test_authorized_rejects_malformed_base64() {
    assert!(!authorized(Some("Basic !!!not-base64!!!"), "s3cret"));
}

#[test]
fn test_authorized_rejects_non_utf8_and_missing_colon() {
    assert!(!authorized(Some(&basic(&[0xff, 0xfe])), "s3cret"));
    assert!(!authorized(Some(&basic(b"nocolonhere")), "s3cret"));
}

#[test]
fn test_parse_basic_extracts_user_and_password() {
    let parsed = parse_basic(&basic(b"alice:s3cret")).unwrap();
    assert_eq!(parsed.user, "alice");
    assert_eq!(parsed.password, "s3cret");
}
