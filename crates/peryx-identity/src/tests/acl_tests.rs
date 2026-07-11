use std::collections::BTreeSet;

use rstest::rstest;

use crate::{Action, Denial, Glob, Grant, IndexAcl, NamedToken, Principal, authorize};

use super::basic;

fn grant(projects: &[&str], actions: &[Action]) -> Grant {
    Grant {
        projects: projects.iter().copied().map(Glob::new).collect(),
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
#[case::empty_project("*", "", true)]
fn test_glob_matches(#[case] pattern: &str, #[case] project: &str, #[case] expected: bool) {
    assert_eq!(Glob::new(pattern).matches(project), expected);
}

#[test]
fn test_authorize_grants_a_named_token_its_projects() {
    let acl = acl(vec![token("ci", "s3cret", grant(&["team/*"], &[Action::Write]))]);
    assert_eq!(authorize(&subject("ci"), &acl, Some("team/api"), Action::Write), Ok(()));
}

#[test]
fn test_authorize_refuses_a_project_outside_the_grant() {
    let acl = acl(vec![token("ci", "s3cret", grant(&["team/*"], &[Action::Write]))]);
    assert_eq!(
        authorize(&subject("ci"), &acl, Some("other/api"), Action::Write),
        Err(Denial::Forbidden)
    );
}

#[test]
fn test_authorize_refuses_an_action_outside_the_grant() {
    let acl = acl(vec![token("ci", "s3cret", grant(&["*"], &[Action::Write]))]);
    assert_eq!(
        authorize(&subject("ci"), &acl, Some("team/api"), Action::Delete),
        Err(Denial::Forbidden)
    );
}

#[test]
fn test_authorize_refuses_a_subject_the_index_does_not_know() {
    let acl = acl(vec![token("ci", "s3cret", grant(&["*"], &[Action::Write]))]);
    assert_eq!(
        authorize(&subject("ghost"), &acl, Some("team/api"), Action::Write),
        Err(Denial::Forbidden)
    );
}

#[test]
fn test_authorize_without_a_project_asks_whether_any_project_is_open() {
    let acl = acl(vec![token("ci", "s3cret", grant(&["team/*"], &[Action::Write]))]);
    assert_eq!(authorize(&subject("ci"), &acl, None, Action::Write), Ok(()));
    assert_eq!(
        authorize(&subject("ci"), &acl, None, Action::Delete),
        Err(Denial::Forbidden)
    );
}

#[test]
fn test_authorize_tells_an_anonymous_write_to_authenticate() {
    let acl = acl(vec![token("ci", "s3cret", grant(&["*"], &[Action::Write]))]);
    assert_eq!(
        authorize(&Principal::Anonymous, &acl, Some("team/api"), Action::Write),
        Err(Denial::Unauthenticated)
    );
}

#[test]
fn test_authorize_reports_an_action_no_token_grants_as_unavailable() {
    let write_only = acl(vec![token("ci", "s3cret", grant(&["*"], &[Action::Write]))]);
    assert_eq!(
        authorize(&Principal::Anonymous, &write_only, Some("team/api"), Action::Delete),
        Err(Denial::Unavailable)
    );
    assert_eq!(
        authorize(&Principal::Anonymous, &acl(Vec::new()), None, Action::Write),
        Err(Denial::Unavailable)
    );
}

#[test]
fn test_authorize_lets_anyone_read_by_default() {
    let acl = IndexAcl::default();
    assert!(acl.anonymous_read);
    assert_eq!(
        authorize(&Principal::Anonymous, &acl, Some("team/api"), Action::Read),
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
        authorize(&Principal::Anonymous, &closed, Some("team/api"), Action::Read),
        Err(Denial::Unauthenticated)
    );
    assert_eq!(
        authorize(&subject("ci"), &closed, Some("team/api"), Action::Read),
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
    let header = basic(b"__token__:s3cret");
    assert_eq!(acl.identify(Some(&header), 99).principal, subject("ci"));
    assert_eq!(acl.identify(Some(&header), 100).principal, Principal::Anonymous);
}

#[test]
fn test_upload_token_grants_writes_and_deletes_everywhere() {
    let acl = IndexAcl::upload_token("s3cret");
    let principal = acl.identify(Some(&basic(b"__token__:s3cret")), 0).principal;
    assert_eq!(authorize(&principal, &acl, Some("anything"), Action::Write), Ok(()));
    assert_eq!(authorize(&principal, &acl, Some("anything"), Action::Delete), Ok(()));
}

#[test]
fn test_grants_are_the_named_token_s_and_anonymous_holds_none() {
    let write = grant(&["team/*"], &[Action::Write]);
    let acl = acl(vec![token("ci", "s3cret", write.clone())]);
    assert_eq!(acl.grants(&subject("ci")), [write]);
    assert!(acl.grants(&subject("ghost")).is_empty());
    assert!(acl.grants(&Principal::Anonymous).is_empty());
}
