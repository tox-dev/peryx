use std::collections::BTreeSet;

use rstest::rstest;

use crate::{
    Action, Denial, Glob, Grant, IndexAcl, NamedToken, Principal, ResourceMatch, authorize, authorize_all,
    authorize_named_all,
};

use super::basic;

fn grant(resources: &[&str], actions: &[Action]) -> Grant {
    Grant {
        resources: resources.iter().copied().map(Glob::new).collect(),
        actions: actions.iter().copied().collect::<BTreeSet<_>>(),
    }
}

fn token(name: &str, secret: &str, grant: Grant) -> NamedToken {
    NamedToken {
        name: name.to_owned(),
        secret: secret.to_owned(),
        grants: vec![grant],
        expires_at: None,
    }
}

fn acl(tokens: Vec<NamedToken>) -> IndexAcl {
    IndexAcl {
        anonymous_read: true,
        tokens,
    }
}

fn subject(name: &str) -> Principal {
    Principal::Named {
        subject: name.to_owned(),
    }
}

#[rstest]
#[case::literal("team/api", "team/api", true)]
#[case::literal_miss("team/api", "team/web", false)]
#[case::segment("team/*", "team/api", true)]
#[case::multi_segment("team/*", "team/api/edge", true)]
#[case::prefix_of_another_team("team/*", "teamwork/api", false)]
#[case::shallower("team/*", "team", false)]
#[case::everything("*", "anything/at/all", true)]
#[case::suffix("*-internal", "acme-internal", true)]
#[case::suffix_miss("*-internal", "acme-public", false)]
#[case::two_stars("*/build/*", "team/build/nightly", true)]
#[case::two_stars_miss("*/build/*", "team/release/nightly", false)]
#[case::empty_resource("*", "", true)]
fn test_glob_matches(#[case] pattern: &str, #[case] resource: &str, #[case] expected: bool) {
    assert_eq!(Glob::new(pattern).matches(resource), expected);
}

#[rstest]
#[case::literal_extension("images/app", "images/", true)]
#[case::literal_exhausted("images", "images/", false)]
#[case::wildcard_extension("images/team/*", "images/", true)]
#[case::wildcard_consumes_prefix("*/app", "images/", true)]
#[case::different_prefix("other/*", "images/", false)]
fn test_glob_matches_prefix(#[case] pattern: &str, #[case] prefix: &str, #[case] expected: bool) {
    assert_eq!(Glob::new(pattern).matches_prefix(prefix), expected);
}

#[test]
fn test_glob_preserves_its_pattern() {
    assert_eq!(Glob::new("team/*").as_str(), "team/*");
}

#[rstest]
#[case::literal("images/app", "images/", &["app"])]
#[case::wildcard("images/team/*", "images/", &["team/*"])]
#[case::wildcard_prefix("*/app", "images/", &["*/app", "/app", "app"])]
#[case::root("app", "", &["app"])]
#[case::miss("other/*", "images/", &[])]
fn test_glob_remainders_after(#[case] pattern: &str, #[case] prefix: &str, #[case] expected: &[&str]) {
    assert_eq!(
        Glob::new(pattern).remainders_after(prefix).collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn test_authorize_grants_a_named_token_its_resources() {
    let acl = acl(vec![token("ci", "s3cret", grant(&["team/*"], &[Action::Write]))]);
    assert_eq!(
        authorize(&subject("ci"), &acl, ResourceMatch::Pattern("team/api"), Action::Write),
        Ok(())
    );
}

#[test]
fn test_authorize_refuses_a_resource_outside_the_grant() {
    let acl = acl(vec![token("ci", "s3cret", grant(&["team/*"], &[Action::Write]))]);
    assert_eq!(
        authorize(&subject("ci"), &acl, ResourceMatch::Pattern("other/api"), Action::Write),
        Err(Denial::Forbidden)
    );
}

#[test]
fn test_authorize_refuses_an_action_outside_the_grant() {
    let acl = acl(vec![token("ci", "s3cret", grant(&["*"], &[Action::Write]))]);
    assert_eq!(
        authorize(&subject("ci"), &acl, ResourceMatch::Pattern("team/api"), Action::Delete),
        Err(Denial::Forbidden)
    );
}

#[test]
fn test_authorize_refuses_a_subject_the_index_does_not_know() {
    let acl = acl(vec![token("ci", "s3cret", grant(&["*"], &[Action::Write]))]);
    assert_eq!(
        authorize(
            &subject("ghost"),
            &acl,
            ResourceMatch::Pattern("team/api"),
            Action::Write
        ),
        Err(Denial::Forbidden)
    );
}

#[test]
fn test_authorize_without_a_resource_asks_whether_any_resource_is_open() {
    let acl = acl(vec![token("ci", "s3cret", grant(&["team/*"], &[Action::Write]))]);
    assert_eq!(
        authorize(&subject("ci"), &acl, ResourceMatch::Any, Action::Write),
        Ok(())
    );
    assert_eq!(
        authorize(&subject("ci"), &acl, ResourceMatch::Any, Action::Delete),
        Err(Denial::Forbidden)
    );
}

#[rstest]
#[case::all_resources("ci", "*", Action::Read, Ok(()))]
#[case::narrow_grant("ci", "team/*", Action::Read, Err(Denial::Forbidden))]
#[case::wrong_action("ci", "*", Action::Write, Err(Denial::Forbidden))]
#[case::unknown_subject("other", "*", Action::Read, Err(Denial::Forbidden))]
fn test_authorize_all_requires_a_matching_wildcard_grant(
    #[case] principal: &str,
    #[case] resources: &str,
    #[case] action: Action,
    #[case] expected: Result<(), Denial>,
) {
    let acl = IndexAcl {
        anonymous_read: false,
        tokens: vec![token("ci", "s3cret", grant(&[resources], &[Action::Read]))],
    };

    assert_eq!(authorize_all(&subject(principal), &acl, action), expected);
}

/// Anonymous readability opens artifact serving to callers who present nothing; it never widens what
/// a presented credential may reach, so the token's own grants decide either way.
#[rstest]
#[case::all_resources("ci", "*", Action::Read, Ok(()))]
#[case::narrow_grant("ci", "team/*", Action::Read, Err(Denial::Forbidden))]
#[case::wrong_action("ci", "*", Action::Write, Err(Denial::Forbidden))]
#[case::unknown_subject("other", "*", Action::Read, Err(Denial::Forbidden))]
fn test_authorize_named_all_ignores_anonymous_read(
    #[case] principal: &str,
    #[case] resources: &str,
    #[case] action: Action,
    #[case] expected: Result<(), Denial>,
) {
    let acl = acl(vec![token("ci", "s3cret", grant(&[resources], &[Action::Read]))]);

    assert_eq!(authorize_named_all(principal, &acl, action), expected);
}

#[rstest]
#[case::public(true, true, None, Ok(()))]
#[case::credential_required(false, true, Some(i64::MAX), Err(Denial::Unauthenticated))]
#[case::expired(false, true, Some(0), Err(Denial::Unavailable))]
#[case::unavailable(false, false, None, Err(Denial::Unavailable))]
fn test_authorize_all_classifies_anonymous_reads(
    #[case] anonymous_read: bool,
    #[case] token_can_read: bool,
    #[case] expires_at: Option<i64>,
    #[case] expected: Result<(), Denial>,
) {
    let tokens = token_can_read.then(|| NamedToken {
        expires_at,
        ..token("ci", "s3cret", grant(&["*"], &[Action::Read]))
    });
    let acl = IndexAcl {
        anonymous_read,
        tokens: tokens.into_iter().collect(),
    };

    assert_eq!(authorize_all(&Principal::Anonymous, &acl, Action::Read), expected);
}

#[rstest]
#[case::exact("artifact:inventory", Action::Read, Ok(()))]
#[case::glob_does_not_expand("other", Action::Read, Err(Denial::Forbidden))]
#[case::wrong_action("artifact:inventory", Action::Write, Err(Denial::Forbidden))]
fn test_exact_resource_matching_does_not_expand_globs(
    #[case] resource: &str,
    #[case] action: Action,
    #[case] expected: Result<(), Denial>,
) {
    let grants = [grant(&["artifact:inventory", "*"], &[Action::Read])];

    assert_eq!(
        crate::authorize_grants(&grants, ResourceMatch::Exact(resource), action),
        expected
    );
}

#[rstest]
#[case::active(i64::MAX, Err(Denial::Unauthenticated))]
#[case::expired(0, Err(Denial::Unavailable))]
fn test_authorize_classifies_anonymous_by_live_grants(#[case] expires_at: i64, #[case] expected: Result<(), Denial>) {
    let acl = acl(vec![NamedToken {
        expires_at: Some(expires_at),
        ..token("ci", "s3cret", grant(&["*"], &[Action::Write]))
    }]);
    assert_eq!(
        authorize(
            &Principal::Anonymous,
            &acl,
            ResourceMatch::Pattern("team/api"),
            Action::Write
        ),
        expected
    );
}

#[test]
fn test_authorize_reports_an_action_no_token_grants_as_unavailable() {
    let write_only = acl(vec![token("ci", "s3cret", grant(&["*"], &[Action::Write]))]);
    assert_eq!(
        authorize(
            &Principal::Anonymous,
            &write_only,
            ResourceMatch::Pattern("team/api"),
            Action::Delete
        ),
        Err(Denial::Unavailable)
    );
    assert_eq!(
        authorize(
            &Principal::Anonymous,
            &acl(Vec::new()),
            ResourceMatch::Any,
            Action::Write
        ),
        Err(Denial::Unavailable)
    );
}

#[test]
fn test_authorize_lets_anyone_read_by_default() {
    let acl = IndexAcl::default();
    assert!(acl.anonymous_read);
    assert_eq!(
        authorize(
            &Principal::Anonymous,
            &acl,
            ResourceMatch::Pattern("team/api"),
            Action::Read
        ),
        Ok(())
    );
}

#[test]
fn test_authorize_refuses_an_anonymous_read_when_the_index_is_closed() {
    let closed = IndexAcl {
        anonymous_read: false,
        tokens: vec![token("ci", "s3cret", grant(&["*"], &[Action::Read]))],
    };
    assert_eq!(
        authorize(
            &Principal::Anonymous,
            &closed,
            ResourceMatch::Pattern("team/api"),
            Action::Read
        ),
        Err(Denial::Unauthenticated)
    );
    assert_eq!(
        authorize(
            &subject("ci"),
            &closed,
            ResourceMatch::Pattern("team/api"),
            Action::Read
        ),
        Ok(())
    );
}

#[test]
fn test_identify_ignores_an_expired_token() {
    let expiring = NamedToken {
        expires_at: Some(100),
        ..token("ci", "s3cret", grant(&["*"], &[Action::Write]))
    };
    let acl = acl(vec![expiring]);
    let header = basic(b"client:s3cret");
    assert_eq!(acl.identify(Some(&header), 99).principal, subject("ci"));
    assert_eq!(acl.identify(Some(&header), 100).principal, Principal::Anonymous);
}

#[rstest]
#[case::unbounded(None, i64::MAX, true)]
#[case::before_expiry(Some(100), 99, true)]
#[case::at_expiry(Some(100), 100, false)]
fn test_grants_to_anyone_at_uses_live_tokens(
    #[case] expires_at: Option<i64>,
    #[case] now: i64,
    #[case] expected: bool,
) {
    let acl = acl(vec![NamedToken {
        expires_at,
        ..token("ci", "s3cret", grant(&["*"], &[Action::Write]))
    }]);

    assert_eq!(acl.grants_to_anyone_at(Action::Write, now), expected);
}

#[test]
fn test_write_and_delete_grant_covers_every_resource() {
    let acl = acl(vec![token(
        "uploader",
        "s3cret",
        grant(&["*"], &[Action::Write, Action::Delete]),
    )]);
    let principal = acl.identify(Some(&basic(b"client:s3cret")), 0).principal;
    assert_eq!(
        authorize(&principal, &acl, ResourceMatch::Pattern("anything"), Action::Write),
        Ok(())
    );
    assert_eq!(
        authorize(&principal, &acl, ResourceMatch::Pattern("anything"), Action::Delete),
        Ok(())
    );
}

#[test]
fn test_grants_are_the_named_token_s_and_anonymous_holds_none() {
    let write = grant(&["team/*"], &[Action::Write]);
    let acl = acl(vec![token("ci", "s3cret", write.clone())]);
    assert_eq!(acl.grants(&subject("ci")), [write]);
    assert!(acl.grants(&subject("ghost")).is_empty());
    assert!(acl.grants(&Principal::Anonymous).is_empty());
}
