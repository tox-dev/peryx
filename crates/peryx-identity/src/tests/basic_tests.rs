use rstest::rstest;

use crate::{IndexAcl, Principal, parse_basic};

use super::basic;

fn named(acl: &IndexAcl, header: &str) -> Principal {
    acl.identify(Some(header), 0).principal
}

#[test]
fn test_identify_accepts_any_user_with_the_token() {
    let acl = IndexAcl::upload_token("s3cret");
    let subject = Principal::Named {
        subject: "upload_token".to_owned(),
    };
    assert_eq!(named(&acl, &basic(b"__token__:s3cret")), subject);
    assert_eq!(named(&acl, &basic(b"alice:s3cret")), subject);
}

#[test]
fn test_identify_rejects_wrong_password() {
    // A shorter guess exercises the length short-circuit; a same-length guess exercises the
    // byte-by-byte constant-time comparison to its end.
    let acl = IndexAcl::upload_token("s3cret");
    assert_eq!(named(&acl, &basic(b"alice:nope")), Principal::Anonymous);
    assert_eq!(named(&acl, &basic(b"alice:s3crXt")), Principal::Anonymous);
}

#[test]
fn test_identify_rejects_missing_or_non_basic_header() {
    let acl = IndexAcl::upload_token("s3cret");
    assert_eq!(acl.identify(None, 0).principal, Principal::Anonymous);
    assert_eq!(named(&acl, "Bearer s3cret"), Principal::Anonymous);
}

#[test]
fn test_identify_rejects_malformed_base64() {
    let acl = IndexAcl::upload_token("s3cret");
    assert_eq!(named(&acl, "Basic !!!not-base64!!!"), Principal::Anonymous);
}

#[test]
fn test_identify_rejects_non_utf8_and_missing_colon() {
    let acl = IndexAcl::upload_token("s3cret");
    assert_eq!(named(&acl, &basic(&[0xff, 0xfe])), Principal::Anonymous);
    assert_eq!(named(&acl, &basic(b"nocolonhere")), Principal::Anonymous);
}

#[test]
fn test_identify_keeps_the_presented_user_whatever_the_verdict() {
    let acl = IndexAcl::upload_token("s3cret");
    assert_eq!(
        acl.identify(Some(&basic(b"alice:nope")), 0).user.as_deref(),
        Some("alice")
    );
    assert_eq!(acl.identify(None, 0).user, None);
}

#[rstest]
#[case::canonical("Basic")]
#[case::lower("basic")]
#[case::mixed("bAsIc")]
fn test_parse_basic_extracts_credentials_for_case_insensitive_scheme(#[case] scheme: &str) {
    let header = basic(b"alice:s3cret").replacen("Basic", scheme, 1);
    let parsed = parse_basic(&header).unwrap();

    assert_eq!((parsed.user.as_str(), parsed.password.as_str()), ("alice", "s3cret"));
}
