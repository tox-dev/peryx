use std::collections::{BTreeMap, BTreeSet};

use peryx_identity::{Action, Glob, Grant};
use rstest::rstest;

use super::{PublishClaims, PublishDenial, TrustedPublisher, authorize_publish};

const NOW: i64 = 1_000;
const ISSUER: &str = "https://token.actions.githubusercontent.com";
const AUDIENCE: &str = "peryx";

fn publisher() -> TrustedPublisher {
    TrustedPublisher {
        issuer: ISSUER.to_owned(),
        audience: AUDIENCE.to_owned(),
        subject: Glob::new("repo:octo/app:*"),
        claims: BTreeMap::from([("repository".to_owned(), "octo/app".to_owned())]),
        projects: vec![Glob::new("app")],
    }
}

fn claims() -> PublishClaims {
    PublishClaims {
        issuer: ISSUER.to_owned(),
        audience: AUDIENCE.to_owned(),
        subject: "repo:octo/app:ref:refs/heads/main".to_owned(),
        expires_at: NOW + 300,
        claims: BTreeMap::from([("repository".to_owned(), "octo/app".to_owned())]),
    }
}

#[test]
fn test_matching_identity_returns_its_rule_and_grant() {
    assert_eq!(
        authorize_publish(&[publisher()], &claims(), NOW),
        Ok((
            0,
            vec![Grant {
                resources: vec![Glob::new("app")],
                actions: BTreeSet::from([Action::Write]),
            }],
        ))
    );
}

#[test]
fn test_no_publishers_rejects_the_issuer() {
    assert_eq!(
        authorize_publish(&[], &claims(), NOW),
        Err(PublishDenial::UnknownIssuer)
    );
}

#[rstest]
#[case::issuer(
    PublishClaims { issuer: "https://gitlab.example/oidc".to_owned(), ..claims() },
    PublishDenial::UnknownIssuer,
)]
#[case::audience(PublishClaims { audience: "other".to_owned(), ..claims() }, PublishDenial::WrongAudience)]
#[case::expiry(PublishClaims { expires_at: NOW, ..claims() }, PublishDenial::Expired)]
#[case::subject(
    PublishClaims { subject: "repo:octo/other:ref:refs/heads/main".to_owned(), ..claims() },
    PublishDenial::WrongSubject,
)]
#[case::claim(
    PublishClaims { claims: BTreeMap::from([("repository".to_owned(), "octo/fork".to_owned())]), ..claims() },
    PublishDenial::ClaimMismatch { claim: "repository".to_owned() },
)]
#[case::missing_claim(
    PublishClaims { claims: BTreeMap::new(), ..claims() },
    PublishDenial::ClaimMismatch { claim: "repository".to_owned() },
)]
fn test_mismatched_identity_is_rejected(#[case] presented: PublishClaims, #[case] expected: PublishDenial) {
    assert_eq!(authorize_publish(&[publisher()], &presented, NOW), Err(expected));
}

#[test]
fn test_publisher_without_extra_claims_matches() {
    assert!(
        authorize_publish(
            &[TrustedPublisher {
                claims: BTreeMap::new(),
                ..publisher()
            }],
            &claims(),
            NOW
        )
        .is_ok()
    );
}

#[rstest]
#[case::specific_first(vec![wrong_subject_rule(), other_issuer_rule()])]
#[case::specific_last(vec![other_issuer_rule(), wrong_subject_rule()])]
fn test_most_specific_denial_is_order_independent(#[case] publishers: Vec<TrustedPublisher>) {
    assert_eq!(
        authorize_publish(&publishers, &claims(), NOW),
        Err(PublishDenial::WrongSubject)
    );
}

#[test]
fn test_later_matching_publisher_reports_its_position() {
    assert_eq!(
        authorize_publish(&[other_issuer_rule(), publisher()], &claims(), NOW)
            .unwrap()
            .0,
        1
    );
}

#[test]
fn test_denials_explain_the_failure() {
    assert_eq!(
        PublishDenial::ClaimMismatch {
            claim: "environment".to_owned(),
        }
        .to_string(),
        "the token is missing the required claim `environment` or carries a different value"
    );
    assert_eq!(PublishDenial::Expired.to_string(), "the token has expired");
}

fn wrong_subject_rule() -> TrustedPublisher {
    TrustedPublisher {
        subject: Glob::new("repo:octo/other:*"),
        ..publisher()
    }
}

fn other_issuer_rule() -> TrustedPublisher {
    TrustedPublisher {
        issuer: "https://gitlab.example/oidc".to_owned(),
        ..publisher()
    }
}
