use std::collections::BTreeSet;

use rstest::rstest;

use crate::{Action, Glob, Grant, IndexAcl, NamedToken, Principal, parse_basic};

use super::basic;

fn writer_acl(secret: impl Into<String>) -> IndexAcl {
    IndexAcl {
        anonymous_read: true,
        tokens: vec![NamedToken {
            name: "uploader".to_owned(),
            secret: secret.into(),
            grants: vec![Grant {
                resources: vec![Glob::new("*")],
                actions: BTreeSet::from([Action::Write, Action::Delete]),
            }],
            expires_at: None,
        }],
    }
}

fn named(acl: &IndexAcl, header: &str) -> Principal {
    acl.identify(Some(header), 0).principal
}

#[test]
fn test_identify_accepts_any_user_with_the_token() {
    let acl = writer_acl("s3cret");
    let subject = Principal::Named {
        subject: "uploader".to_owned(),
    };
    assert_eq!(named(&acl, &basic(b"client:s3cret")), subject);
    assert_eq!(named(&acl, &basic(b"alice:s3cret")), subject);
}

#[test]
fn test_identify_rejects_wrong_password() {
    let acl = writer_acl("s3cret");
    assert_eq!(named(&acl, &basic(b"alice:nope")), Principal::Anonymous);
    assert_eq!(named(&acl, &basic(b"alice:s3crXt")), Principal::Anonymous);
}

#[test]
fn test_identify_rejects_missing_or_non_basic_header() {
    let acl = writer_acl("s3cret");
    assert_eq!(acl.identify(None, 0).principal, Principal::Anonymous);
    assert_eq!(named(&acl, "Bearer s3cret"), Principal::Anonymous);
}

#[test]
fn test_identify_rejects_malformed_base64() {
    let acl = writer_acl("s3cret");
    assert_eq!(named(&acl, "Basic !!!not-base64!!!"), Principal::Anonymous);
}

#[test]
fn test_identify_rejects_non_utf8_and_missing_colon() {
    let acl = writer_acl("s3cret");
    assert_eq!(named(&acl, &basic(&[0xff, 0xfe])), Principal::Anonymous);
    assert_eq!(named(&acl, &basic(b"nocolonhere")), Principal::Anonymous);
}

#[test]
fn test_identify_keeps_the_presented_user_whatever_the_verdict() {
    let acl = writer_acl("s3cret");
    assert_eq!(
        acl.identify(Some(&basic(b"alice:nope")), 0).presented_user.as_deref(),
        Some("alice")
    );
    assert_eq!(acl.identify(None, 0).presented_user, None);
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
